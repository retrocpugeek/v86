//! Sv39 address translation, a software TLB, and memory accesses.

use crate::cpu::*;
use crate::csr::SATP_MODE_SV39;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Acc {
    Read,
    Write,
    Exec,
}

// PTE bits
const PTE_V: u64 = 1;
const PTE_R: u64 = 2;
const PTE_W: u64 = 4;
const PTE_X: u64 = 8;
const PTE_U: u64 = 16;
const PTE_A: u64 = 64;
const PTE_D: u64 = 128;

const TLB_ENTRIES: usize = 512;

#[derive(Clone, Copy)]
struct TlbEntry {
    /// va >> 12, or u64::MAX
    vpn: u64,
    /// Physical address of the page.
    pa: u64,
    /// The leaf PTE's bits (D set only if it was set when cached).
    pte: u64,
}

pub struct Tlb {
    e: Vec<TlbEntry>,
}

impl Default for Tlb {
    fn default() -> Self { Tlb { e: vec![TlbEntry { vpn: u64::MAX, pa: 0, pte: 0 }; TLB_ENTRIES] } }
}

impl Tlb {
    pub fn flush(&mut self) {
        for e in self.e.iter_mut() {
            e.vpn = u64::MAX;
        }
    }
}

fn page_fault(acc: Acc, va: u64) -> Exception {
    let cause = match acc {
        Acc::Read => cause::LOAD_PAGE_FAULT,
        Acc::Write => cause::STORE_PAGE_FAULT,
        Acc::Exec => cause::INSN_PAGE_FAULT,
    };
    Exception { cause, tval: va }
}

pub fn access_fault(acc: Acc, va: u64) -> Exception {
    let cause = match acc {
        Acc::Read => cause::LOAD_ACCESS,
        Acc::Write => cause::STORE_ACCESS,
        Acc::Exec => cause::INSN_ACCESS,
    };
    Exception { cause, tval: va }
}

impl Cpu {
    /// The privilege level loads and stores (not fetches) use: MPRV.
    #[inline]
    fn data_prv(&self) -> u64 {
        if self.prv == PRV_M && self.csr.mstatus & MSTATUS_MPRV != 0 {
            (self.csr.mstatus & MSTATUS_MPP) >> MSTATUS_MPP_SHIFT
        } else {
            self.prv
        }
    }

    /// Is a leaf PTE's permission enough for this access?
    fn pte_allows(&self, pte: u64, acc: Acc, prv: u64) -> bool {
        let s = self.csr.mstatus;
        if pte & PTE_U != 0 {
            if prv == PRV_S && (acc == Acc::Exec || s & MSTATUS_SUM == 0) {
                return false;
            }
        } else if prv == PRV_U {
            return false;
        }
        match acc {
            Acc::Read => pte & PTE_R != 0 || (s & MSTATUS_MXR != 0 && pte & PTE_X != 0),
            Acc::Write => pte & PTE_W != 0,
            Acc::Exec => pte & PTE_X != 0,
        }
    }

    /// Translate va for an access; physical address.
    #[inline]
    pub fn translate(&mut self, va: u64, acc: Acc) -> Res<u64> {
        let prv = if acc == Acc::Exec { self.prv } else { self.data_prv() };
        if prv == PRV_M || self.csr.satp >> 60 != SATP_MODE_SV39 {
            return Ok(va);
        }
        let vpn = va >> 12;
        let e = self.tlb.e[(vpn as usize) % TLB_ENTRIES];
        if e.vpn == vpn && self.pte_allows(e.pte, acc, prv) && (acc != Acc::Write || e.pte & PTE_D != 0) {
            return Ok(e.pa | (va & 0xfff));
        }
        self.walk(va, acc, prv)
    }

    #[inline(never)]
    fn walk(&mut self, va: u64, acc: Acc, prv: u64) -> Res<u64> {
        // va must be the sign extension of bit 38
        if ((va as i64) << 25 >> 25) as u64 != va {
            return Err(page_fault(acc, va));
        }
        let mut table = (self.csr.satp & ((1 << 44) - 1)) << 12;
        for level in (0..3).rev() {
            let idx = va >> (12 + 9 * level) & 0x1ff;
            let pte_addr = table + idx * 8;
            let Some(off) = self.bus.ram_offset(pte_addr, 8) else { return Err(access_fault(acc, va)) };
            let pte = self.bus.read_ram(off, 8);
            if pte & PTE_V == 0 || (pte & PTE_R == 0 && pte & PTE_W != 0) || pte >> 54 != 0 {
                return Err(page_fault(acc, va));
            }
            let ppn = pte >> 10 & ((1 << 44) - 1);
            if pte & (PTE_R | PTE_X) == 0 {
                table = ppn << 12;
                continue;
            }
            // Leaf: superpages must be aligned
            if ppn & ((1 << (9 * level)) - 1) != 0 {
                return Err(page_fault(acc, va));
            }
            if !self.pte_allows(pte, acc, prv) {
                return Err(page_fault(acc, va));
            }
            // A and D are set by the hardware (QEMU's default)
            let mut new = pte | PTE_A;
            if acc == Acc::Write {
                new |= PTE_D;
            }
            if new != pte {
                self.bus.write_ram(off, 8, new);
            }
            let page_mask = (1u64 << (12 + 9 * level)) - 1;
            let pa_page = ((ppn << 12) & !page_mask) | (va & page_mask & !0xfff);
            self.tlb.e[((va >> 12) as usize) % TLB_ENTRIES] = TlbEntry { vpn: va >> 12, pa: pa_page, pte: new };
            return Ok(pa_page | (va & 0xfff));
        }
        Err(page_fault(acc, va))
    }

    /// Read from physical memory; access fault outside RAM and devices.
    fn phys_read(&mut self, pa: u64, size: u32, va: u64, acc: Acc) -> Res<u64> {
        if let Some(off) = self.bus.ram_offset(pa, size as u64) {
            return Ok(self.bus.read_ram(off, size));
        }
        if !self.bus.has_device(pa) {
            return Err(access_fault(acc, va));
        }
        self.bus.now_ns = self.now_ns();
        let v = self.bus.read(pa, size);
        self.update_mip();
        Ok(v)
    }

    fn phys_write(&mut self, pa: u64, size: u32, v: u64, va: u64) -> Res<()> {
        if let Some(off) = self.bus.ram_offset(pa, size as u64) {
            self.bus.write_ram(off, size, v);
            return Ok(());
        }
        if !self.bus.has_device(pa) {
            return Err(access_fault(Acc::Write, va));
        }
        self.bus.now_ns = self.now_ns();
        self.bus.write(pa, size, v);
        match self.bus.power_request {
            crate::bus::PowerRequest::None => {}
            crate::bus::PowerRequest::Off => self.power = Power::Off,
            crate::bus::PowerRequest::Reset => self.power = Power::Reset,
        }
        self.update_mip();
        Ok(())
    }

    /// Load `size` bytes from va (zero-extended). Misaligned loads are
    /// handled here, a byte at a time if they cross a page.
    #[inline]
    pub fn load(&mut self, va: u64, size: u32) -> Res<u64> {
        if va & (size as u64 - 1) == 0 || (va & 0xfff) + size as u64 <= 0x1000 {
            let pa = self.translate(va, Acc::Read)?;
            return self.phys_read(pa, size, va, Acc::Read);
        }
        let mut v = 0;
        for i in 0..size as u64 {
            let a = va.wrapping_add(i);
            let pa = self.translate(a, Acc::Read)?;
            v |= self.phys_read(pa, 1, a, Acc::Read)? << (8 * i);
        }
        Ok(v)
    }

    #[inline]
    pub fn store(&mut self, va: u64, size: u32, v: u64) -> Res<()> {
        if va & (size as u64 - 1) == 0 || (va & 0xfff) + size as u64 <= 0x1000 {
            let pa = self.translate(va, Acc::Write)?;
            return self.phys_write(pa, size, v, va);
        }
        // Translate both pages first so a fault leaves memory unchanged
        self.translate(va, Acc::Write)?;
        self.translate(va.wrapping_add(size as u64 - 1), Acc::Write)?;
        for i in 0..size as u64 {
            let a = va.wrapping_add(i);
            let pa = self.translate(a, Acc::Write)?;
            self.phys_write(pa, 1, v >> (8 * i) & 0xff, a)?;
        }
        Ok(())
    }

    /// Translate an aligned atomic access (AMO, LR, SC); physical address.
    pub fn amo_addr(&mut self, va: u64, size: u32, acc: Acc) -> Res<u64> {
        if va & (size as u64 - 1) != 0 {
            let cause = if acc == Acc::Read { cause::LOAD_MISALIGNED } else { cause::STORE_MISALIGNED };
            return Err(Exception { cause, tval: va });
        }
        let pa = self.translate(va, acc)?;
        if self.bus.ram_offset(pa, size as u64).is_none() {
            return Err(access_fault(acc, va));
        }
        Ok(pa)
    }

    fn fetch16(&mut self, va: u64) -> Res<u64> {
        let pa = self.translate(va, Acc::Exec)?;
        if let Some(off) = self.bus.ram_offset(pa, 2) {
            return Ok(self.bus.read_ram(off, 2));
        }
        if self.bus.has_rom(pa) {
            return Ok(self.bus.read(pa, 2));
        }
        Err(access_fault(Acc::Exec, va))
    }

    /// Fetch the instruction at pc: (bits, length in bytes).
    #[inline]
    pub fn fetch(&mut self, pc: u64) -> Res<(u32, u64)> {
        if pc & 1 != 0 {
            return Err(Exception { cause: cause::INSN_MISALIGNED, tval: pc });
        }
        if pc & 0xfff <= 0xffc {
            let pa = self.translate(pc, Acc::Exec)?;
            if let Some(off) = self.bus.ram_offset(pa, 4) {
                let w = self.bus.read_ram(off, 4) as u32;
                return Ok(if w & 3 == 3 { (w, 4) } else { (w & 0xffff, 2) });
            }
        }
        let lo = self.fetch16(pc)? as u32;
        if lo & 3 != 3 {
            return Ok((lo, 2));
        }
        // The upper half may be on the next page
        let hi = self.fetch16(pc.wrapping_add(2)).map_err(|e| {
            // A fault on the second half reports the instruction's address
            // plus 2 (as QEMU does)
            e
        })? as u32;
        Ok((lo | hi << 16, 4))
    }
}
