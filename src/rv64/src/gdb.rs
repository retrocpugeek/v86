//! A GDB remote serial protocol stub for the machine, as a debug probe on a
//! board would be: gdb connects (`target remote`), stops the hart, reads and
//! writes registers and memory, steps, and sets breakpoints anywhere —
//! in flash too, since they're the core's own (Cpu::breakpoints) rather than
//! ebreaks written into the program.
//!
//! Independent of the transport: input() takes the bytes gdb sent and leaves
//! the reply in `out`; poll() runs the hart while gdb has it running and
//! reports the stop. rv64run serves it on a TCP port (--gdb); the emulator
//! pages pass it a connection from the guest that runs gdb.
//!
//! The target description says what the hart is (RV32E's 16 registers, or
//! 32; F registers when it has them), so gdb doesn't have to guess it from
//! the ELF.

use crate::cpu::{Cpu, RunExit};

/// What the hart is doing, as far as gdb is concerned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Stopped, for gdb to look at.
    Halted,
    /// Running until a breakpoint, an ebreak, or gdb interrupts it.
    Running,
    /// One instruction, then stop.
    Stepping,
}

/// The CSRs offered to gdb, in its numbering (65 + the CSR's number).
const CSRS: &[(&str, u32)] = &[
    ("mstatus", 0x300),
    ("misa", 0x301),
    ("mie", 0x304),
    ("mtvec", 0x305),
    ("mscratch", 0x340),
    ("mepc", 0x341),
    ("mcause", 0x342),
    ("mtval", 0x343),
    ("mip", 0x344),
];
const CSR_BASE: usize = 65;
const PC: usize = 32;
const FPR_BASE: usize = 33;

pub struct Gdb {
    pub mode: Mode,
    /// Bytes for gdb.
    pub out: Vec<u8>,
    /// A packet being received.
    buf: Vec<u8>,
    in_packet: bool,
    no_ack: bool,
    /// The hart has 16 integer registers (RV32E), or 32.
    embedded: bool,
    fpu: bool,
    tdesc: String,
}

impl Gdb {
    /// Attaches to the hart, which stops: a debugger connecting halts it, and
    /// gdb asks why before anything else. `embedded` is RV32E's 16 integer
    /// registers; `fpu` single-precision F registers (a CH32V307 has them, a
    /// CH32V003 doesn't — misa says neither, being the core's).
    pub fn attach(cpu: &mut Cpu, embedded: bool, fpu: bool) -> Gdb {
        cpu.debug_ebreak = true;
        cpu.debug_stop = false;
        let tdesc = target_description(cpu.xlen32, embedded, fpu);
        Gdb { mode: Mode::Halted, out: Vec::new(), buf: Vec::new(), in_packet: false, no_ack: false, embedded, fpu, tdesc }
    }

    /// Lets the hart run on undisturbed, as a debugger that goes away does.
    pub fn detach(&mut self, cpu: &mut Cpu) {
        cpu.breakpoints.clear();
        cpu.debug_ebreak = false;
        cpu.debug_stop = false;
        cpu.debug_resume = None;
        self.mode = Mode::Running;
        resume_clock(cpu);
    }

    /// The bytes gdb sent.
    pub fn input(&mut self, cpu: &mut Cpu, data: &[u8]) {
        for &b in data {
            if self.in_packet {
                if b == b'#' {
                    // The checksum's two digits follow; it's a reliable
                    // transport, so they're not checked, and they're skipped
                    // as stray bytes below.
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
                self.stop(cpu, "T02");
            }
        }
    }

    /// Runs the hart for up to `budget` steps while gdb has it running, and
    /// reports a stop. Returns what run() did, or None when halted.
    pub fn poll(&mut self, cpu: &mut Cpu, budget: u64) -> Option<RunExit> {
        match self.mode {
            Mode::Halted => None,
            Mode::Stepping => {
                // Interrupts aren't taken while stepping, as with dcsr.stepie
                // clear: `next` stays in the function rather than wandering
                // into whatever timer handler was due.
                let irq = cpu.auto_irq;
                cpu.auto_irq = false;
                cpu.debug_resume = None;
                cpu.step();
                cpu.auto_irq = irq;
                cpu.debug_stop = false;
                self.stop(cpu, "T05");
                Some(RunExit::Debug)
            }
            Mode::Running => {
                let r = cpu.run(budget);
                if matches!(r, RunExit::Debug) {
                    cpu.debug_stop = false;
                    self.stop(cpu, "T05swbreak:;");
                }
                Some(r)
            }
        }
    }

    fn stop(&mut self, _cpu: &mut Cpu, reply: &str) {
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

    fn resume(&mut self, cpu: &mut Cpu, step: bool, addr: Option<u64>) {
        if let Some(a) = addr {
            cpu.pc = a;
        }
        cpu.debug_stop = false;
        cpu.debug_resume = Some(cpu.pc);
        cpu.stop_batch = true;
        resume_clock(cpu);
        self.mode = if step { Mode::Stepping } else { Mode::Running };
    }

    fn packet(&mut self, cpu: &mut Cpu, p: &[u8]) {
        let s = String::from_utf8_lossy(p).into_owned();
        let (cmd, rest) = s.split_at(1.min(s.len()));
        match cmd {
            "?" => self.reply("T05"),
            "g" => {
                let mut r = String::new();
                for n in 0..if self.embedded { 16 } else { 32 } {
                    r += &self.reg_hex(cpu, n).unwrap_or_default();
                }
                r += &self.reg_hex(cpu, PC).unwrap_or_default();
                self.reply(&r);
            }
            "G" => {
                let w = if cpu.xlen32 { 8 } else { 16 };
                let regs: Vec<usize> =
                    (0..if self.embedded { 16 } else { 32 }).chain(std::iter::once(PC)).collect();
                for (i, n) in regs.into_iter().enumerate() {
                    if let Some(h) = rest.get(i * w..i * w + w) {
                        self.set_reg(cpu, n, h);
                    }
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
                    for i in 0..len as u64 {
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
                // Binary: escaped with '}' (and the byte xor 0x20)
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
                // Z0/Z1 (software and hardware breakpoints alike: the core's
                // own); watchpoints aren't offered, and gdb then does them by
                // single-stepping
                let mut f = rest.split(',');
                let kind = f.next().unwrap_or("");
                let addr = f.next().and_then(|a| u64::from_str_radix(a, 16).ok());
                match (kind, addr) {
                    ("0" | "1", Some(a)) => {
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
                    // One hart: the first action is the one
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
            "q" | "Q" => self.query(cpu, &s),
            _ => self.reply(""),
        }
    }

    fn query(&mut self, _cpu: &mut Cpu, s: &str) {
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
                // '$', '#', '}' and '*' are escaped in a binary reply
                if matches!(b, b'$' | b'#' | b'}' | b'*') {
                    r.push(b'}');
                    r.push(b ^ 0x20);
                } else {
                    r.push(b);
                }
            }
            self.send(&r);
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

    fn width(&self, cpu: &Cpu) -> usize { if cpu.xlen32 { 4 } else { 8 } }

    /// A register in gdb's numbering, as the little-endian hex gdb reads.
    fn reg_hex(&self, cpu: &mut Cpu, n: usize) -> Option<String> {
        let w = self.width(cpu);
        let v = match n {
            0..=31 if !self.embedded || n < 16 => cpu.x[n],
            PC => cpu.pc,
            _ if self.fpu && (FPR_BASE..FPR_BASE + 32).contains(&n) => {
                return Some(le_hex(cpu.f[n - FPR_BASE], 4));
            }
            _ if n >= CSR_BASE => {
                let csr = (n - CSR_BASE) as u32;
                if !CSRS.iter().any(|&(_, c)| c == csr) {
                    return None;
                }
                cpu.csr_read(csr).ok()?
            }
            _ => return None,
        };
        Some(le_hex(v, w))
    }

    fn set_reg(&self, cpu: &mut Cpu, n: usize, hex: &str) -> bool {
        let Some(bytes) = unhex(hex) else { return false };
        let mut v = 0u64;
        for (i, &b) in bytes.iter().enumerate().take(8) {
            v |= (b as u64) << (8 * i);
        }
        if cpu.xlen32 && n != PC && n < 32 {
            v = v as u32 as i32 as i64 as u64;
        }
        match n {
            0 => true,
            1..=31 if !self.embedded || n < 16 => {
                cpu.x[n] = v;
                true
            }
            PC => {
                cpu.pc = if cpu.xlen32 { v as u32 as u64 } else { v };
                true
            }
            _ if self.fpu && (FPR_BASE..FPR_BASE + 32).contains(&n) => {
                // NaN-boxed, as the core keeps single-precision values
                cpu.f[n - FPR_BASE] = v | 0xffff_ffff_0000_0000;
                true
            }
            _ if n >= CSR_BASE && CSRS.iter().any(|&(_, c)| c == (n - CSR_BASE) as u32) => {
                cpu.csr_write((n - CSR_BASE) as u32, v).is_ok()
            }
            _ => false,
        }
    }
}

/// A halted hart's clock doesn't run: on resume, real time is measured from
/// now, rather than the hart racing to make up the time it spent stopped.
fn resume_clock(cpu: &mut Cpu) {
    cpu.host_base_ns = None;
}

fn resume_addr(cmd: &str, rest: &str) -> Option<u64> {
    // c [addr], C sig[;addr]
    let a = if cmd == "C" || cmd == "S" { rest.split_once(';').map(|(_, a)| a)? } else { rest };
    u64::from_str_radix(a, 16).ok()
}

fn parse_addr_len(s: &str) -> Option<(u64, usize)> {
    let (a, l) = s.split_once(',')?;
    Some((u64::from_str_radix(a, 16).ok()?, usize::from_str_radix(l, 16).ok()?.min(0x10000)))
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

fn le_hex(v: u64, bytes: usize) -> String {
    (0..bytes).map(|i| format!("{:02x}", (v >> (8 * i)) as u8)).collect()
}

/// A byte as the debugger sees it: RAM and flash directly, and peripherals
/// through the bus, with whatever a read of them does — as a probe's read
/// does on the board.
fn peek(cpu: &mut Cpu, pa: u64) -> u8 {
    let pa = if cpu.xlen32 { pa as u32 as u64 } else { pa };
    if let Some(off) = cpu.bus.ram_offset(pa, 1) {
        return cpu.bus.ram[off];
    }
    if let Some(off) = cpu.bus.rom_offset(pa) {
        return cpu.bus.rom.get(off).copied().unwrap_or(0);
    }
    if !is_peripheral(cpu, pa) {
        // Nothing there: gdb's unwinder reads past the stack's top, and a
        // board's bus returns something rather than complaining
        return 0;
    }
    cpu.bus.read(pa & !3, 4).to_le_bytes()[(pa & 3) as usize]
}

/// A modelled peripheral is at pa (only the CH32V's are known; elsewhere the
/// bus is read anyway).
fn is_peripheral(cpu: &Cpu, pa: u64) -> bool {
    match cpu.bus.ch32.as_ref() {
        Some(c) => c.claims(pa),
        None => true,
    }
}

/// Writes bytes for the debugger: RAM, flash (gdb's `load`, or a patch), and
/// peripherals a byte at a time.
fn poke_all(cpu: &mut Cpu, pa: u64, data: &[u8]) {
    let mut code = false;
    for (i, &b) in data.iter().enumerate() {
        let a = pa.wrapping_add(i as u64);
        let a = if cpu.xlen32 { a as u32 as u64 } else { a };
        if let Some(off) = cpu.bus.ram_offset(a, 1) {
            cpu.bus.write_ram(off, 1, b as u64);
            code = true;
        } else if let Some(off) = cpu.bus.rom_offset(a) {
            if let Some(slot) = cpu.bus.rom.get_mut(off) {
                *slot = b;
                code = true;
            }
        } else if is_peripheral(cpu, a) {
            cpu.bus.write(a, 1, b as u64);
        }
    }
    if code {
        // Decoded or compiled copies of what was written
        cpu.icache_flush();
    }
}

fn target_description(xlen32: bool, embedded: bool, fpu: bool) -> String {
    let bits = if xlen32 { 32 } else { 64 };
    let mut x = String::new();
    let names = [
        "zero", "ra", "sp", "gp", "tp", "t0", "t1", "t2", "fp", "s1", "a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7",
        "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9", "s10", "s11", "t3", "t4", "t5", "t6",
    ];
    for (i, n) in names.iter().enumerate().take(if embedded { 16 } else { 32 }) {
        let ty = match i {
            1 => "code_ptr",
            2 | 3 | 4 | 8 => "data_ptr",
            _ => "int",
        };
        x += &format!("<reg name=\"{n}\" bitsize=\"{bits}\" regnum=\"{i}\" type=\"{ty}\"/>");
    }
    x += &format!("<reg name=\"pc\" bitsize=\"{bits}\" regnum=\"{PC}\" type=\"code_ptr\"/>");
    let mut doc = format!(
        "<?xml version=\"1.0\"?><!DOCTYPE target SYSTEM \"gdb-target.dtd\"><target version=\"1.0\">\
         <architecture>riscv:rv{bits}</architecture><feature name=\"org.gnu.gdb.riscv.cpu\">{x}</feature>"
    );
    if fpu {
        doc += "<feature name=\"org.gnu.gdb.riscv.fpu\">";
        for i in 0..32 {
            doc += &format!("<reg name=\"f{i}\" bitsize=\"32\" regnum=\"{}\" type=\"ieee_single\"/>", FPR_BASE + i);
        }
        doc += "</feature>";
    }
    doc += "<feature name=\"org.gnu.gdb.riscv.csr\">";
    for &(name, csr) in CSRS {
        doc += &format!("<reg name=\"{name}\" bitsize=\"{bits}\" regnum=\"{}\"/>", CSR_BASE + csr as usize);
    }
    doc += "</feature></target>";
    doc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dev::ch32::{machine, Chip};

    /// A CH32V003 running `loop: addi a0, a0, 1; j loop`, with an ebreak at
    /// 8 that only a jump gets to.
    fn hart() -> Cpu {
        let mut fw = Vec::new();
        for insn in [0x0015_0513u32, 0xffdf_f06f, 0x0010_0073] {
            fw.extend_from_slice(&insn.to_le_bytes());
        }
        machine(Chip::V003, &fw, Box::new(|_| {}), 0).unwrap()
    }

    /// Sends one packet and returns the packets that came back.
    fn ask(g: &mut Gdb, cpu: &mut Cpu, p: &str) -> Vec<String> {
        let sum = p.bytes().fold(0u8, |s, b| s.wrapping_add(b));
        g.input(cpu, format!("${p}#{sum:02x}").as_bytes());
        replies(g)
    }

    fn replies(g: &mut Gdb) -> Vec<String> {
        let out = String::from_utf8_lossy(&std::mem::take(&mut g.out)).into_owned();
        out.split('$').skip(1).map(|p| p.split('#').next().unwrap().to_string()).collect()
    }

    fn reg(g: &mut Gdb, cpu: &mut Cpu, n: usize) -> u32 {
        let h = &ask(g, cpu, &format!("p{n:x}"))[0];
        u32::from_le_bytes(unhex(h).unwrap().try_into().unwrap())
    }

    #[test]
    fn halts_reads_breaks_steps_and_interrupts() {
        let mut cpu = hart();
        let mut g = Gdb::attach(&mut cpu, true, false);
        assert_eq!(ask(&mut g, &mut cpu, "QStartNoAckMode"), ["OK"]);
        assert_eq!(ask(&mut g, &mut cpu, "?"), ["T05"]);
        // RV32E: 16 registers and the pc, 8 hex digits each
        assert_eq!(ask(&mut g, &mut cpu, "g")[0].len(), 17 * 8);
        // Flash, through its alias at 0
        assert_eq!(ask(&mut g, &mut cpu, "m0,4"), ["13051500"]);

        // A breakpoint in flash, hit on every pass round the loop
        assert_eq!(ask(&mut g, &mut cpu, "Z0,4,2"), ["OK"]);
        for pass in 1..=3 {
            assert!(ask(&mut g, &mut cpu, "c").is_empty());
            g.poll(&mut cpu, 1000);
            assert_eq!(replies(&mut g), ["T05swbreak:;"]);
            assert_eq!(reg(&mut g, &mut cpu, PC), 4);
            assert_eq!(reg(&mut g, &mut cpu, 10), pass);
        }
        assert_eq!(ask(&mut g, &mut cpu, "z0,4,2"), ["OK"]);
        ask(&mut g, &mut cpu, "s");
        g.poll(&mut cpu, 1000);
        assert_eq!(replies(&mut g), ["T05"]);
        assert_eq!(reg(&mut g, &mut cpu, PC), 0);

        // Running freely until ^C
        ask(&mut g, &mut cpu, "vCont;c");
        g.poll(&mut cpu, 1000);
        assert!(replies(&mut g).is_empty());
        g.input(&mut cpu, &[3]);
        assert_eq!(replies(&mut g), ["T02"]);
        assert!(reg(&mut g, &mut cpu, 10) > 3);
    }

    #[test]
    fn writes_registers_ram_and_flash() {
        let mut cpu = hart();
        let mut g = Gdb::attach(&mut cpu, true, false);
        assert_eq!(ask(&mut g, &mut cpu, "Pa=78563412"), ["OK"]);
        assert_eq!(reg(&mut g, &mut cpu, 10), 0x1234_5678);
        // Registers RV32E doesn't have
        assert_eq!(ask(&mut g, &mut cpu, "p10"), ["E01"]);
        assert_eq!(ask(&mut g, &mut cpu, "M20000000,4:efbeadde"), ["OK"]);
        assert_eq!(ask(&mut g, &mut cpu, "m20000000,4"), ["efbeadde"]);
        // Patching flash replaces the decoded instruction: addi a0, a0, 2
        assert_eq!(ask(&mut g, &mut cpu, "M0,4:13052500"), ["OK"]);
        ask(&mut g, &mut cpu, "Pa=00000000");
        ask(&mut g, &mut cpu, "s");
        g.poll(&mut cpu, 10);
        replies(&mut g);
        assert_eq!(reg(&mut g, &mut cpu, 10), 2);
        // Nothing mapped: zeros, rather than an error the unwinder trips on
        assert_eq!(ask(&mut g, &mut cpu, "mfffffffc,4"), ["00000000"]);
    }

    #[test]
    fn ebreak_stops_while_attached_and_traps_after() {
        let mut cpu = hart();
        let mut g = Gdb::attach(&mut cpu, true, false);
        ask(&mut g, &mut cpu, "P20=08000000");
        ask(&mut g, &mut cpu, "c");
        g.poll(&mut cpu, 1000);
        assert_eq!(replies(&mut g), ["T05swbreak:;"]);
        assert_eq!(reg(&mut g, &mut cpu, PC), 8);
        // Detached, it is the program's breakpoint exception again
        ask(&mut g, &mut cpu, "D");
        cpu.run(1);
        assert_eq!(cpu.csr.mcause, 3);
        assert!(!cpu.debug_ebreak && cpu.breakpoints.is_empty());
    }

    #[test]
    fn describes_the_hart() {
        let mut cpu = hart();
        let mut g = Gdb::attach(&mut cpu, true, false);
        let x = &ask(&mut g, &mut cpu, "qXfer:features:read:target.xml:0,ffff")[0];
        assert!(x.starts_with('l') && x.contains("riscv:rv32") && x.contains("name=\"a5\""));
        assert!(!x.contains("name=\"a6\"") && !x.contains("org.gnu.gdb.riscv.fpu"));
        let mut g = Gdb::attach(&mut cpu, false, true);
        let x = &ask(&mut g, &mut cpu, "qXfer:features:read:target.xml:0,ffff")[0];
        assert!(x.contains("name=\"t6\"") && x.contains("name=\"f31\""));
    }
}
