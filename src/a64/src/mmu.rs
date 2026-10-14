//! Stage-1 EL1&0 translation (VMSAv8-64, 4 KB granule) and virtual memory access.

use crate::bits::ones;
use crate::cpu::Cpu;
use crate::decode::{decode, Op};
use crate::exc::{ec, fsc, Exception, Res};

const TLB_SIZE: usize = 4096;

pub const P_R1: u8 = 1;
pub const P_W1: u8 = 2;
pub const P_X1: u8 = 4;
pub const P_R0: u8 = 8;
pub const P_W0: u8 = 16;
pub const P_X0: u8 = 32;

#[derive(Clone, Copy)]
struct Entry {
    vpn: u64,
    pa_page: u64,
    asid: u16,
    global: bool,
    perm: u8,
    /// Offset of the page in Bus::ram, or NOT_RAM (device memory).
    ram: u32,
}

const NOT_RAM: u32 = u32::MAX;
const INVALID: Entry = Entry { vpn: u64::MAX, pa_page: 0, asid: 0, global: false, perm: 0, ram: NOT_RAM };

pub struct Tlb {
    e: Box<[Entry; TLB_SIZE]>,
    /// ASID currently in use (from TTBR0/1 according to TCR.A1).
    pub asid: u16,
    /// Instruction fetch cache: the page (VA) instructions were last fetched
    /// from, if it's RAM, and its offset in Bus::ram. Valid until the TLB is
    /// flushed or the ASID or exception level changes (invalidate_fetch).
    pub(crate) fetch_page: u64,
    pub(crate) fetch_ram: usize,
}

/// TLB layout, for the inline lookups in generated code (jit.rs).
pub(crate) mod layout {
    use super::{Entry, TLB_SIZE};
    use std::mem::{offset_of, size_of};
    pub const ENTRIES: u32 = TLB_SIZE as u32;
    pub const ENTRY_SIZE: u32 = size_of::<Entry>() as u32;
    pub const VPN: u32 = offset_of!(Entry, vpn) as u32;
    pub const ASID: u32 = offset_of!(Entry, asid) as u32;
    pub const GLOBAL: u32 = offset_of!(Entry, global) as u32;
    pub const PERM: u32 = offset_of!(Entry, perm) as u32;
    pub const RAM: u32 = offset_of!(Entry, ram) as u32;
}

impl Default for Tlb {
    fn default() -> Self { Self::new() }
}

impl Tlb {
    pub fn new() -> Tlb { Tlb { e: Box::new([INVALID; TLB_SIZE]), asid: 0, fetch_page: u64::MAX, fetch_ram: 0 } }

    pub fn flush(&mut self) {
        self.e.fill(INVALID);
        self.invalidate_fetch();
    }

    #[inline(always)]
    pub fn invalidate_fetch(&mut self) { self.fetch_page = u64::MAX; }

    /// Put an entry for `va` -> `pa` with the current ASID (tests).
    pub fn insert(&mut self, va: u64, pa: u64, global: bool, perm: u8, ram: Option<usize>) {
        let vpn = va >> 12;
        self.e[Self::index(vpn)] = Entry {
            vpn,
            pa_page: pa & !0xfff,
            asid: self.asid,
            global,
            perm,
            ram: ram.map_or(NOT_RAM, |r| r as u32),
        };
    }

    /// Address of the entries (they're never reallocated).
    pub(crate) fn entries_addr(&self) -> usize { self.e.as_ptr() as usize }

    /// Diagnostics: the entry that would serve `va`, as (pa_page, asid, global, perm).
    pub fn probe(&self, va: u64) -> Option<(u64, u16, bool, u8)> {
        let vpn = va >> 12;
        let e = self.e[Self::index(vpn)];
        (e.vpn == vpn).then_some((e.pa_page, e.asid, e.global, e.perm))
    }

    #[inline(always)]
    fn index(vpn: u64) -> usize {
        // jit.rs generates the same
        ((vpn ^ (vpn >> 12)) as usize) & (TLB_SIZE - 1)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Acc {
    Read,
    Write,
    Exec,
}

pub struct Walk {
    pub pa: u64,
    pub perm: u8,
    pub global: bool,
    pub level: u32,
}

impl Cpu {
    #[inline(always)]
    pub fn mmu_on(&self) -> bool { self.sr.sctlr_el1 & 1 != 0 }

    pub fn update_asid(&mut self) {
        let tcr = self.sr.tcr_el1;
        let ttbr = if tcr >> 22 & 1 != 0 { self.sr.ttbr1_el1 } else { self.sr.ttbr0_el1 };
        let asid = (ttbr >> 48) as u16;
        self.tlb.asid = if tcr >> 36 & 1 != 0 { asid } else { asid & 0xff };
        self.tlb.invalidate_fetch();
    }

    /// Table walk for `va`; Err holds the fault status code.
    pub fn walk(&mut self, va: u64, acc: Acc) -> Result<Walk, u32> {
        let tcr = self.sr.tcr_el1;
        let upper = va >> 55 & 1 != 0;
        let (tsz, ttbr, epd, tbi, tbid, tg) = if upper {
            (tcr >> 16 & 0x3f, self.sr.ttbr1_el1, tcr >> 23 & 1, tcr >> 38 & 1, tcr >> 52 & 1, tcr >> 30 & 3)
        } else {
            (tcr & 0x3f, self.sr.ttbr0_el1, tcr >> 7 & 1, tcr >> 37 & 1, tcr >> 51 & 1, tcr >> 14 & 3)
        };
        let tsz = tsz.clamp(16, 39) as u32;
        let va_bits = 64 - tsz;
        let tbi = tbi != 0 && !(acc == Acc::Exec && tbid != 0);
        let top = if tbi { 56 } else { 64 };
        let field = (va >> va_bits) & ones(top - va_bits);
        let expect = if upper { ones(top - va_bits) } else { 0 };
        if field != expect || epd != 0 {
            return Err(fsc::TRANSLATION);
        }
        let granule_ok = if upper { tg == 2 } else { tg == 0 };
        if !granule_ok {
            eprintln!("a64: unsupported translation granule (tcr={tcr:#x})");
            return Err(fsc::TRANSLATION);
        }

        let start = if va_bits > 39 {
            0
        } else if va_bits > 30 {
            1
        } else {
            2
        };
        let mut level = start;
        let mut table = ttbr & 0x0000_ffff_ffff_fffe;
        let mut ap_table = 0u64;
        let mut uxn_table = false;
        let mut pxn_table = false;
        loop {
            let shift = 39 - 9 * level;
            let idx_bits = if level == start { (va_bits - shift).min(9) } else { 9 };
            let idx = (va >> shift) & ones(idx_bits);
            let desc = self.bus.read(table + idx * 8, 8);
            if desc & 1 == 0 {
                return Err(fsc::TRANSLATION + level);
            }
            if level < 3 && desc & 2 != 0 {
                table = desc & 0x0000_ffff_ffff_f000;
                ap_table |= desc >> 61 & 3;
                uxn_table |= desc >> 60 & 1 != 0;
                pxn_table |= desc >> 59 & 1 != 0;
                level += 1;
                continue;
            }
            if level == 0 || (level == 3 && desc & 2 == 0) {
                return Err(fsc::TRANSLATION + level);
            }
            if desc & (1 << 10) == 0 {
                return Err(fsc::ACCESS_FLAG + level);
            }
            let oa = desc & 0x0000_ffff_ffff_f000 & !ones(shift);
            let pa = oa | (va & ones(shift));
            let ap = desc >> 6 & 3;
            let el0_ok = ap & 1 != 0 && ap_table & 1 == 0;
            let ro = ap & 2 != 0 || ap_table & 2 != 0;
            let uxn = desc >> 54 & 1 != 0 || uxn_table;
            let pxn = desc >> 53 & 1 != 0 || pxn_table;
            let wxn = self.sr.sctlr_el1 >> 19 & 1 != 0;
            let el0_w = el0_ok && !ro;
            let mut perm = P_R1;
            if !ro {
                perm |= P_W1;
            }
            if el0_ok {
                perm |= P_R0;
                if !ro {
                    perm |= P_W0;
                }
            }
            // EL1 may not execute memory that EL0 can write.
            if !pxn && !el0_w && !(wxn && !ro) {
                perm |= P_X1;
            }
            if !uxn && !(wxn && el0_w) {
                perm |= P_X0;
            }
            return Ok(Walk { pa: pa & !0xfff, perm, global: desc >> 11 & 1 == 0, level });
        }
    }

    #[inline(always)]
    fn needed_perm(&self, acc: Acc, unpriv: bool) -> u8 {
        let el0 = self.el == 0 || unpriv;
        match (acc, el0) {
            (Acc::Read, false) => P_R1,
            (Acc::Read, true) => P_R0,
            (Acc::Write, false) => P_W1,
            (Acc::Write, true) => P_W0,
            (Acc::Exec, false) => P_X1,
            (Acc::Exec, true) => P_X0,
        }
    }

    fn abort(&self, va: u64, acc: Acc, fsc: u32, cm: bool) -> Exception {
        let lower = self.el == 0;
        let (ec, mut iss) = match acc {
            Acc::Exec => (if lower { ec::IABT_LOWER } else { ec::IABT_CUR }, fsc),
            _ => (if lower { ec::DABT_LOWER } else { ec::DABT_CUR }, fsc),
        };
        if acc == Acc::Write || cm {
            iss |= 1 << 6;
        }
        if cm {
            iss |= 1 << 8;
        }
        Exception { ec, iss, far: Some(va), ret_next: false }
    }

    /// Translate a virtual address, raising the appropriate abort.
    #[inline(always)]
    pub fn translate(&mut self, va: u64, acc: Acc, unpriv: bool) -> Res<u64> {
        if !self.mmu_on() {
            if va >> 40 != 0 {
                return Err(self.abort(va, acc, fsc::ADDR_SIZE, false));
            }
            return Ok(va);
        }
        let need = self.needed_perm(acc, unpriv);
        let vpn = va >> 12;
        let i = Tlb::index(vpn);
        let e = self.tlb.e[i];
        if e.vpn == vpn && (e.global || e.asid == self.tlb.asid) && e.perm & need != 0 {
            return Ok(e.pa_page | (va & 0xfff));
        }
        self.translate_slow(va, acc, need, false)
    }

    #[cold]
    fn translate_slow(&mut self, va: u64, acc: Acc, need: u8, cm: bool) -> Res<u64> {
        match self.walk(va, acc) {
            Ok(w) => {
                let vpn = va >> 12;
                let ram = self.bus.ram_offset(w.pa, 0x1000).map_or(NOT_RAM, |off| off as u32);
                self.tlb.e[Tlb::index(vpn)] =
                    Entry { vpn, pa_page: w.pa, asid: self.tlb.asid, global: w.global, perm: w.perm, ram };
                if w.perm & need == 0 {
                    return Err(self.abort(va, acc, fsc::PERMISSION + w.level, cm));
                }
                Ok(w.pa | (va & 0xfff))
            }
            Err(f) => Err(self.abort(va, acc, f, cm)),
        }
    }

    /// Translation for cache maintenance by VA: permission as `acc`, faults report CM.
    pub fn translate_cm(&mut self, va: u64, acc: Acc) -> Res<u64> {
        if !self.mmu_on() {
            return Ok(va);
        }
        let need = self.needed_perm(acc, false);
        self.translate_slow(va, acc, need, true)
    }

    // ---- data access ----

    /// Offset in Bus::ram of `va` if the TLB has an entry for it that allows
    /// `acc` and maps RAM: the fast path for loads and stores.
    #[inline(always)]
    fn ram_hit(&self, va: u64, acc: Acc, unpriv: bool) -> Option<usize> {
        if !self.mmu_on() {
            return None;
        }
        let vpn = va >> 12;
        let e = &self.tlb.e[Tlb::index(vpn)];
        if e.vpn == vpn
            && (e.global || e.asid == self.tlb.asid)
            && e.perm & self.needed_perm(acc, unpriv) != 0
            && e.ram != NOT_RAM
        {
            Some(e.ram as usize | (va & 0xfff) as usize)
        } else {
            None
        }
    }

    #[inline(always)]
    pub(crate) fn ram_hit_read(&self, va: u64) -> Option<usize> { self.ram_hit(va, Acc::Read, false) }

    #[inline(always)]
    pub(crate) fn ram_hit_write(&self, va: u64) -> Option<usize> { self.ram_hit(va, Acc::Write, false) }

    /// Access of `size` bytes, which must not cross a page.
    #[inline(always)]
    pub fn read(&mut self, va: u64, size: u32, unpriv: bool) -> Res<u64> {
        if (va & 0xfff) + size as u64 > 0x1000 {
            return self.read_split(va, size, unpriv);
        }
        if let Some(off) = self.ram_hit(va, Acc::Read, unpriv) {
            return Ok(self.bus.read_ram(off, size));
        }
        let pa = self.translate(va, Acc::Read, unpriv)?;
        Ok(self.bus.read(pa, size))
    }

    #[inline(always)]
    pub fn write(&mut self, va: u64, size: u32, v: u64, unpriv: bool) -> Res<()> {
        if (va & 0xfff) + size as u64 > 0x1000 {
            return self.write_split(va, size, v, unpriv);
        }
        if let Some(off) = self.ram_hit(va, Acc::Write, unpriv) {
            self.bus.write_ram(off, size, v);
            return Ok(());
        }
        let pa = self.translate(va, Acc::Write, unpriv)?;
        self.bus.write(pa, size, v);
        Ok(())
    }

    #[cold]
    fn read_split(&mut self, va: u64, size: u32, unpriv: bool) -> Res<u64> {
        let mut v = 0;
        for i in 0..size {
            let a = va.wrapping_add(i as u64);
            let pa = self.translate(a, Acc::Read, unpriv)?;
            v |= self.bus.read(pa, 1) << (8 * i);
        }
        Ok(v)
    }

    #[cold]
    fn write_split(&mut self, va: u64, size: u32, v: u64, unpriv: bool) -> Res<()> {
        // Check both pages before writing anything.
        self.translate(va, Acc::Write, unpriv)?;
        self.translate(va.wrapping_add(size as u64 - 1), Acc::Write, unpriv)?;
        for i in 0..size {
            let a = va.wrapping_add(i as u64);
            let pa = self.translate(a, Acc::Write, unpriv)?;
            self.bus.write(pa, 1, v >> (8 * i));
        }
        Ok(())
    }

    pub fn read128(&mut self, va: u64) -> Res<u128> {
        let lo = self.read(va, 8, false)?;
        let hi = self.read(va.wrapping_add(8), 8, false)?;
        Ok(lo as u128 | (hi as u128) << 64)
    }

    pub fn write128(&mut self, va: u64, v: u128) -> Res<()> {
        self.translate(va, Acc::Write, false)?;
        self.translate(va.wrapping_add(15), Acc::Write, false)?;
        self.write(va, 8, v as u64, false)?;
        self.write(va.wrapping_add(8), 8, (v >> 64) as u64, false)
    }

    /// Read of `size` bytes (1..=16) into a vector-sized value.
    pub fn read_v(&mut self, va: u64, size: u32) -> Res<u128> {
        if size == 16 { self.read128(va) } else { Ok(self.read(va, size, false)? as u128) }
    }

    pub fn write_v(&mut self, va: u64, size: u32, v: u128) -> Res<()> {
        if size == 16 { self.write128(va, v) } else { self.write(va, size, v as u64, false) }
    }

    #[inline(always)]
    pub fn fetch(&mut self, va: u64) -> Res<u32> {
        // Same page as the last fetch (fetch_page has the low bits clear, so
        // a misaligned va doesn't match)
        if va & !0xffc == self.tlb.fetch_page {
            return Ok(self.bus.read_ram(self.tlb.fetch_ram | (va & 0xffc) as usize, 4) as u32);
        }
        self.fetch_slow(va)
    }

    /// Fetch and decode, through the decoded-instruction cache.
    #[inline(always)]
    pub fn fetch_op(&mut self, va: u64) -> Res<Op> {
        if va & !0xffc != self.tlb.fetch_page {
            // Fills the fetch cache if the page is RAM
            let insn = self.fetch_slow(va)?;
            if va & !0xffc != self.tlb.fetch_page {
                return Ok(decode(insn));
            }
        }
        let off = (va & 0xffc) as usize;
        let insn = self.bus.read_ram(self.tlb.fetch_ram | off, 4) as u32;
        // Safety: dcur is the decoded page (PAGE_OPS entries) for fetch_page;
        // fetch_slow sets them together.
        let slot = unsafe { &mut *self.dcur.add(off >> 2) };
        if slot.insn != insn {
            self.dcache.redecode(self.tlb.fetch_ram >> 12, off >> 2, slot, insn);
        }
        Ok(*slot)
    }

    #[inline(never)]
    pub(crate) fn fetch_slow(&mut self, va: u64) -> Res<u32> {
        if va & 3 != 0 {
            return Err(Exception { ec: ec::PC_ALIGN, iss: 0, far: Some(va), ret_next: false });
        }
        let pa = self.translate(va, Acc::Exec, false)?;
        if let Some(off) = self.bus.ram_offset(pa & !0xfff, 0x1000) {
            self.tlb.fetch_page = va & !0xfff;
            self.tlb.fetch_ram = off;
            self.dcur = self.dcache.page(off >> 12);
            if self.jit.enabled {
                self.djit = self.dcache.jit_page(off >> 12);
            }
        }
        Ok(self.bus.read(pa, 4) as u32)
    }

    pub fn alignment_fault(&self, va: u64, write: bool) -> Exception {
        self.abort(va, if write { Acc::Write } else { Acc::Read }, fsc::ALIGNMENT, false)
    }

    /// AT S1E{0,1}{R,W}: returns the PAR_EL1 value.
    pub fn address_translate(&mut self, va: u64, el0: bool, write: bool) -> u64 {
        if !self.mmu_on() {
            return va & 0x0000_ffff_ffff_f000;
        }
        let acc = if write { Acc::Write } else { Acc::Read };
        match self.walk(va, acc) {
            Ok(w) => {
                let need = match (write, el0) {
                    (false, false) => P_R1,
                    (false, true) => P_R0,
                    (true, false) => P_W1,
                    (true, true) => P_W0,
                };
                if w.perm & need == 0 {
                    1 | ((fsc::PERMISSION + w.level) as u64) << 1
                } else {
                    w.pa
                }
            }
            Err(f) => 1 | (f as u64) << 1,
        }
    }
}
