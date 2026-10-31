//! A GDB remote serial protocol stub for a core, as a debug probe's (SWD)
//! would be: gdb connects (`target remote`), the core stops, gdb reads and
//! writes its registers and memory, steps it, and sets breakpoints anywhere,
//! flash too, since they're the core's own (Cpu::breakpoints) rather than
//! BKPT instructions written into the program.
//!
//! Independent of the transport and the machine: input() takes the bytes
//! gdb sent and leaves the reply in `out`; the machine runs the core while
//! the stub has it running (running()) and tells it when it stops
//! (stopped()). On the card10 each core has its own stub, so either can be
//! stopped while the other runs on, as on a multi-core chip's debug port.
//!
//! The target description says what the core is: an M-profile core
//! (org.gnu.gdb.arm.m-profile), its system registers and its FPU's.

use crate::cpu::Cpu;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Stopped, for gdb to look at.
    Halted,
    /// Running until a breakpoint, or until gdb interrupts it.
    Running,
    /// One instruction, then stop.
    Stepping,
}

// gdb's register numbers (as the target description gives them)
const XPSR: usize = 16;
const MSP: usize = 17;
const PSP: usize = 18;
const PRIMASK: usize = 19;
const BASEPRI: usize = 20;
const FAULTMASK: usize = 21;
const CONTROL: usize = 22;
const D0: usize = 23;
const FPSCR: usize = 39;
const NUM_REGS: usize = 40;

pub struct Gdb {
    pub mode: Mode,
    /// Bytes for gdb.
    pub out: Vec<u8>,
    buf: Vec<u8>,
    in_packet: bool,
    no_ack: bool,
    fpu: bool,
    tdesc: String,
    /// gdb asked for a reset (`monitor reset`; true: `monitor reset halt`):
    /// the machine resets (and calls reattach())
    pub reset: Option<bool>,
}

impl Gdb {
    /// Attaches to the core, which stops: a debugger connecting halts it,
    /// and gdb asks why before anything else.
    pub fn attach(cpu: &mut Cpu) -> Gdb {
        cpu.debug_resume = None;
        Gdb {
            mode: Mode::Halted,
            out: Vec::new(),
            buf: Vec::new(),
            in_packet: false,
            no_ack: false,
            fpu: cpu.fpu,
            tdesc: target_description(cpu.fpu),
            reset: None,
        }
    }

    /// Lets the core run on undisturbed, as a debugger that goes away does.
    pub fn detach(&mut self, cpu: &mut Cpu) {
        cpu.breakpoints.clear();
        cpu.debug_resume = None;
        cpu.debug_mask_irqs = false;
        self.mode = Mode::Running;
    }

    /// Carries on with the core after a reset: its breakpoints, and stopped
    /// at the reset vector if gdb asked for that, else as it was.
    pub fn reattach(&mut self, cpu: &mut Cpu, breakpoints: Vec<u32>) {
        cpu.breakpoints = breakpoints;
        if self.reset.take() == Some(true) {
            self.mode = Mode::Halted;
        }
    }

    /// The bytes gdb sent.
    pub fn input(&mut self, cpu: &mut Cpu, data: &[u8]) {
        for &b in data {
            if self.in_packet {
                if b == b'#' {
                    // (the checksum's digits follow: a reliable transport, so
                    // they're skipped as stray bytes)
                    self.in_packet = false;
                    let packet = std::mem::take(&mut self.buf);
                    if !self.no_ack {
                        self.out.push(b'+');
                    }
                    self.packet(cpu, &packet);
                } else {
                    self.buf.push(b);
                }
            } else if b == b'$' {
                self.in_packet = true;
                self.buf.clear();
            } else if b == 0x03 && self.mode != Mode::Halted {
                // ^C: stop where it is
                self.stop("T02");
            }
        }
    }

    /// The core stopped while running: at a breakpoint (or a BKPT), or
    /// after the instruction it stepped.
    pub fn stopped(&mut self, cpu: &mut Cpu) {
        cpu.debug_mask_irqs = false;
        let reason = if self.mode == Mode::Stepping { "T05" } else { "T05swbreak:;" };
        self.stop(reason);
    }

    fn stop(&mut self, reply: &str) {
        self.mode = Mode::Halted;
        self.send(reply.as_bytes());
    }

    fn send(&mut self, data: &[u8]) {
        let sum = data.iter().fold(0u8, |s, &b| s.wrapping_add(b));
        self.out.push(b'$');
        self.out.extend_from_slice(data);
        self.out.extend_from_slice(format!("#{sum:02x}").as_bytes());
    }

    fn reply(&mut self, s: &str) { self.send(s.as_bytes()) }

    fn resume(&mut self, cpu: &mut Cpu, step: bool, addr: Option<u32>) {
        if let Some(a) = addr {
            cpu.pc = a & !1;
            cpu.next_pc = cpu.pc;
        }
        cpu.debug_resume = Some(cpu.pc);
        cpu.debug_mask_irqs = step;
        if step {
            // (a step from WFI/WFE: the core's awake for it)
            cpu.sleeping = false;
        }
        self.mode = if step { Mode::Stepping } else { Mode::Running };
    }

    fn packet(&mut self, cpu: &mut Cpu, p: &[u8]) {
        let s = String::from_utf8_lossy(p).into_owned();
        let (cmd, rest) = s.split_at(1.min(s.len()));
        match cmd {
            "?" => self.reply("T05"),
            "g" => {
                let mut r = String::new();
                for n in 0..NUM_REGS {
                    if let Some(h) = self.reg_hex(cpu, n) {
                        r += &h;
                    }
                }
                self.reply(&r);
            }
            "G" => {
                let mut at = 0;
                for n in 0..NUM_REGS {
                    let w = self.reg_size(n) * 2;
                    if w == 0 {
                        continue;
                    }
                    if let Some(h) = rest.get(at..at + w) {
                        self.set_reg(cpu, n, h);
                    }
                    at += w;
                }
                self.reply("OK");
            }
            "p" => match usize::from_str_radix(rest, 16).ok().and_then(|n| self.reg_hex(cpu, n)) {
                Some(h) => self.reply(&h),
                None => self.reply("E01"),
            },
            "P" => {
                let ok = rest
                    .split_once('=')
                    .and_then(|(n, v)| usize::from_str_radix(n, 16).ok().map(|n| self.set_reg(cpu, n, v)))
                    .unwrap_or(false);
                self.reply(if ok { "OK" } else { "E01" });
            }
            "m" => match parse_addr_len(rest) {
                Some((a, len)) => {
                    let mut r = String::with_capacity(len * 2);
                    for i in 0..len as u32 {
                        r += &format!("{:02x}", peek(cpu, a.wrapping_add(i)));
                    }
                    self.reply(&r);
                }
                None => self.reply("E01"),
            },
            "M" => {
                let ok = rest.split_once(':').and_then(|(al, data)| {
                    let (a, len) = parse_addr_len(al)?;
                    let bytes = unhex(data)?;
                    (bytes.len() == len).then(|| poke_all(cpu, a, &bytes))
                });
                self.reply(if ok.is_some() { "OK" } else { "E01" });
            }
            "X" => {
                // binary: escaped with '}' (and the byte xor 0x20)
                let colon = p.iter().position(|&b| b == b':');
                let ok = colon.and_then(|c| {
                    let (a, len) = parse_addr_len(&String::from_utf8_lossy(&p[1..c]))?;
                    let mut bytes = Vec::with_capacity(len);
                    let mut esc = false;
                    for &b in &p[c + 1..] {
                        if esc {
                            bytes.push(b ^ 0x20);
                            esc = false;
                        } else if b == b'}' {
                            esc = true;
                        } else {
                            bytes.push(b);
                        }
                    }
                    (bytes.len() == len).then(|| poke_all(cpu, a, &bytes))
                });
                self.reply(if ok.is_some() { "OK" } else { "E01" });
            }
            "c" | "C" => self.resume(cpu, false, resume_addr(cmd, rest)),
            "s" | "S" => self.resume(cpu, true, resume_addr(cmd, rest)),
            "Z" | "z" => {
                // Z0/Z1: the core's own breakpoints either way; no
                // watchpoints (gdb then single-steps for them)
                let mut f = rest.split(',');
                let kind = f.next().unwrap_or("");
                let addr = f.next().and_then(|a| u32::from_str_radix(a, 16).ok());
                match (kind, addr) {
                    ("0" | "1", Some(a)) => {
                        let a = a & !1;
                        if cmd == "Z" {
                            if !cpu.breakpoints.contains(&a) {
                                cpu.breakpoints.push(a);
                            }
                        } else {
                            cpu.breakpoints.retain(|&b| b != a);
                        }
                        self.reply("OK");
                    }
                    _ => self.reply(""),
                }
            }
            "D" => {
                self.reply("OK");
                self.detach(cpu);
            }
            "k" => self.detach(cpu),
            "H" | "T" => self.reply("OK"),
            "v" => {
                if s == "vCont?" {
                    self.reply("vCont;c;C;s;S");
                } else if let Some(actions) = s.strip_prefix("vCont;") {
                    let a = actions.split(';').next().unwrap_or("");
                    match a.as_bytes().first() {
                        Some(b'c' | b'C') => self.resume(cpu, false, None),
                        Some(b's' | b'S') => self.resume(cpu, true, None),
                        _ => self.reply("E01"),
                    }
                } else {
                    self.reply("");
                }
            }
            "q" | "Q" => self.query(&s),
            _ => self.reply(""),
        }
    }

    fn query(&mut self, s: &str) {
        if s.starts_with("qSupported") {
            self.reply("PacketSize=4000;qXfer:features:read+;QStartNoAckMode+;swbreak+;hwbreak+;vContSupported+");
        } else if s == "QStartNoAckMode" {
            self.reply("OK");
            self.no_ack = true;
        } else if let Some(rest) = s.strip_prefix("qXfer:features:read:target.xml:") {
            let (off, len) = parse_addr_len(rest).unwrap_or((0, 0));
            let doc = self.tdesc.as_bytes();
            let off = (off as usize).min(doc.len());
            let end = (off + len).min(doc.len());
            let mut r = vec![if end == doc.len() { b'l' } else { b'm' }];
            for &b in &doc[off..end] {
                if matches!(b, b'$' | b'#' | b'}' | b'*') {
                    r.push(b'}');
                    r.push(b ^ 0x20);
                } else {
                    r.push(b);
                }
            }
            self.send(&r);
        } else if let Some(hex) = s.strip_prefix("qRcmd,") {
            // monitor <command>
            let cmd: String = (0..hex.len() / 2)
                .filter_map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok())
                .map(char::from)
                .collect();
            let say = |s: &str| "O".to_string() + &s.bytes().map(|b| format!("{b:02x}")).collect::<String>();
            match cmd.trim() {
                "reset" | "reset run" => {
                    self.reset = Some(false);
                    self.reply("OK");
                }
                "reset halt" => {
                    self.reset = Some(true);
                    let m = say("Reset, stopped at the reset vector; `maint flush register-cache` shows its registers.\n");
                    self.reply(&m);
                    self.reply("OK");
                }
                _ => {
                    let m = say("monitor commands: reset [halt|run] (the whole chip)\n");
                    self.reply(&m);
                    self.reply("OK");
                }
            }
        } else if s == "qAttached" {
            self.reply("1");
        } else if s == "qC" {
            self.reply("QC1");
        } else if s == "qfThreadInfo" {
            self.reply("m1");
        } else if s == "qsThreadInfo" {
            self.reply("l");
        } else {
            self.reply("");
        }
    }

    fn reg_size(&self, n: usize) -> usize {
        match n {
            0..=CONTROL => 4,
            D0..FPSCR if self.fpu => 8,
            FPSCR if self.fpu => 4,
            _ => 0,
        }
    }

    /// A register in gdb's numbering, as the little-endian hex gdb reads.
    fn reg_hex(&self, cpu: &mut Cpu, n: usize) -> Option<String> {
        let v: u64 = match n {
            0..=12 | 14 => cpu.r[n] as u64,
            13 => cpu.sp() as u64,
            15 => cpu.pc as u64,
            XPSR => cpu.xpsr() as u64,
            MSP => cpu.msp as u64,
            PSP => cpu.psp as u64,
            PRIMASK => cpu.primask as u64,
            BASEPRI => cpu.basepri as u64,
            FAULTMASK => cpu.faultmask as u64,
            CONTROL => cpu.control as u64,
            D0..FPSCR if self.fpu => {
                let d = n - D0;
                (cpu.s[2 * d] as u64) | (cpu.s[2 * d + 1] as u64) << 32
            }
            FPSCR if self.fpu => cpu.fpscr as u64,
            _ => return None,
        };
        Some(le_hex(v, self.reg_size(n)))
    }

    fn set_reg(&self, cpu: &mut Cpu, n: usize, hex: &str) -> bool {
        let Some(bytes) = unhex(hex) else { return false };
        let mut v = 0u64;
        for (i, &b) in bytes.iter().enumerate().take(8) {
            v |= (b as u64) << (8 * i);
        }
        let w = v as u32;
        match n {
            0..=12 | 14 => cpu.r[n] = w,
            13 => cpu.set_sp(w & !3),
            15 => {
                cpu.pc = w & !1;
                cpu.next_pc = cpu.pc;
            }
            XPSR => cpu.set_xpsr(w),
            MSP => cpu.msp = w & !3,
            PSP => cpu.psp = w & !3,
            PRIMASK => cpu.primask = w & 1 != 0,
            BASEPRI => cpu.basepri = w as u8,
            FAULTMASK => cpu.faultmask = w & 1 != 0,
            CONTROL => cpu.control = w & 7,
            D0..FPSCR if self.fpu => {
                let d = n - D0;
                cpu.s[2 * d] = v as u32;
                cpu.s[2 * d + 1] = (v >> 32) as u32;
            }
            FPSCR if self.fpu => cpu.fpscr = w,
            _ => return false,
        }
        cpu.bus.scs.dirty = true;
        true
    }
}

fn resume_addr(cmd: &str, rest: &str) -> Option<u32> {
    // c [addr], C sig[;addr]
    let a = if cmd == "C" || cmd == "S" { rest.split_once(';').map(|(_, a)| a)? } else { rest };
    u32::from_str_radix(a, 16).ok()
}

fn parse_addr_len(s: &str) -> Option<(u32, usize)> {
    let (a, l) = s.split_once(',')?;
    Some((u32::from_str_radix(a, 16).ok()?, usize::from_str_radix(l, 16).ok()?.min(0x10000)))
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

fn le_hex(v: u64, bytes: usize) -> String { (0..bytes).map(|i| format!("{:02x}", (v >> (8 * i)) as u8)).collect() }

/// A byte as the debugger sees it: through the bus, as a probe's access
/// is (a peripheral's register with whatever its read does).
fn peek(cpu: &mut Cpu, a: u32) -> u8 { cpu.bus.read(a, 1) as u8 }

/// Bytes the debugger writes: memory and peripherals through the bus, and
/// flash too (a patch), which the core itself can't write.
fn poke_all(cpu: &mut Cpu, a: u32, data: &[u8]) {
    for (i, &b) in data.iter().enumerate() {
        let a = a.wrapping_add(i as u32);
        let off = a.wrapping_sub(cpu.bus.flash_base) as usize;
        if off < cpu.bus.flash.len() {
            cpu.bus.flash[off] = b;
        } else {
            cpu.bus.write(a, 1, b as u32);
        }
    }
}

/// The core for gdb: an M-profile core, its system registers, and the
/// FPU's (D registers: gdb shows them as the S registers too).
fn target_description(fpu: bool) -> String {
    let mut x = String::from(
        "<?xml version=\"1.0\"?>\n<!DOCTYPE target SYSTEM \"gdb-target.dtd\">\n<target version=\"1.0\">\n<architecture>arm</architecture>\n<feature name=\"org.gnu.gdb.arm.m-profile\">\n",
    );
    for n in 0..13 {
        x += &format!("<reg name=\"r{n}\" bitsize=\"32\" regnum=\"{n}\"/>\n");
    }
    x += "<reg name=\"sp\" bitsize=\"32\" type=\"data_ptr\" regnum=\"13\"/>\n";
    x += "<reg name=\"lr\" bitsize=\"32\" regnum=\"14\"/>\n";
    x += "<reg name=\"pc\" bitsize=\"32\" type=\"code_ptr\" regnum=\"15\"/>\n";
    x += &format!("<reg name=\"xpsr\" bitsize=\"32\" regnum=\"{XPSR}\"/>\n</feature>\n");
    x += "<feature name=\"org.gnu.gdb.arm.m-system\">\n";
    x += &format!("<reg name=\"msp\" bitsize=\"32\" type=\"data_ptr\" regnum=\"{MSP}\"/>\n");
    x += &format!("<reg name=\"psp\" bitsize=\"32\" type=\"data_ptr\" regnum=\"{PSP}\"/>\n</feature>\n");
    x += "<feature name=\"org.v86.cortex-m.system\">\n";
    for (name, n) in [("primask", PRIMASK), ("basepri", BASEPRI), ("faultmask", FAULTMASK), ("control", CONTROL)] {
        x += &format!("<reg name=\"{name}\" bitsize=\"32\" regnum=\"{n}\" group=\"system\"/>\n");
    }
    x += "</feature>\n";
    if fpu {
        x += "<feature name=\"org.gnu.gdb.arm.vfp\">\n";
        for d in 0..16 {
            x += &format!("<reg name=\"d{d}\" bitsize=\"64\" type=\"ieee_double\" regnum=\"{}\"/>\n", D0 + d);
        }
        x += &format!("<reg name=\"fpscr\" bitsize=\"32\" type=\"int\" group=\"float\" regnum=\"{FPSCR}\"/>\n</feature>\n");
    }
    x + "</target>\n"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::Bus;
    use crate::cpu::Stop;

    /// A core with an FPU and a loop in flash: movs r0, #1; adds r0, #1; b .-2
    fn core() -> Cpu {
        let mut bus = Bus::new(0x1000, 0x1000, 1_000_000, 1_000_000, false);
        let words: [u32; 2] = [0x2000_1000, 0x0000_0009];
        for (i, w) in words.iter().enumerate() {
            bus.flash[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        bus.flash[8..14].copy_from_slice(&[0x01, 0x20, 0x01, 0x30, 0xfd, 0xe7]);
        let mut cpu = Cpu::new(bus);
        cpu.fpu = true;
        cpu
    }

    fn ask(g: &mut Gdb, cpu: &mut Cpu, p: &str) -> String {
        g.out.clear();
        g.input(cpu, format!("${p}#00").as_bytes());
        let out = String::from_utf8(std::mem::take(&mut g.out)).unwrap();
        // the last packet's payload (a continue has none yet)
        match (out.rfind('$'), out.rfind('#')) {
            (Some(a), Some(b)) => out[a + 1..b].to_string(),
            _ => String::new(),
        }
    }

    #[test]
    fn registers_memory_breakpoints_steps() {
        let mut cpu = core();
        let mut g = Gdb::attach(&mut cpu);
        assert_eq!(ask(&mut g, &mut cpu, "?"), "T05");
        // pc (15) and sp (13)
        assert_eq!(ask(&mut g, &mut cpu, "pf"), "08000000");
        assert_eq!(ask(&mut g, &mut cpu, "pd"), "00100020");
        // all of them: 23 words, 16 doubles, fpscr
        assert_eq!(ask(&mut g, &mut cpu, "g").len(), (23 * 4 + 16 * 8 + 4) * 2);
        assert_eq!(ask(&mut g, &mut cpu, "m8,2"), "0120");
        // a breakpoint on the add, and continue
        assert_eq!(ask(&mut g, &mut cpu, "Z0,a,2"), "OK");
        ask(&mut g, &mut cpu, "c");
        assert_eq!(g.mode, Mode::Running);
        assert_eq!(cpu.run(100), Stop::Breakpoint);
        g.stopped(&mut cpu);
        assert_eq!(cpu.pc, 0xa);
        // a step from the breakpoint runs the instruction under it
        ask(&mut g, &mut cpu, "s");
        assert_eq!(cpu.run(1), Stop::Budget);
        g.stopped(&mut cpu);
        assert_eq!((cpu.pc, cpu.r[0]), (0xc, 2));
        // r0 written, d1 written (s2, s3)
        assert_eq!(ask(&mut g, &mut cpu, "P0=2a000000"), "OK");
        assert_eq!(cpu.r[0], 42);
        assert_eq!(ask(&mut g, &mut cpu, "P18=000080bf0000803f"), "OK");
        assert_eq!((cpu.s[2], cpu.s[3]), (0xbf80_0000, 0x3f80_0000));
        // flash patched by the debugger
        assert_eq!(ask(&mut g, &mut cpu, "M8,1:02"), "OK");
        assert_eq!(cpu.bus.flash[8], 2);
    }

    #[test]
    fn describes_an_m_profile_core_with_an_fpu() {
        let d = target_description(true);
        for f in ["org.gnu.gdb.arm.m-profile", "org.gnu.gdb.arm.m-system", "org.gnu.gdb.arm.vfp", "name=\"xpsr\"", "name=\"d15\""] {
            assert!(d.contains(f), "{f}");
        }
    }
}
