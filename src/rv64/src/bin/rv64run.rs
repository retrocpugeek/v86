//! Native runner for the RISC-V core.
//!
//!   rv64run --bios fw_dynamic.bin --kernel Image --dtb virt.dtb [--disk rootfs]
//!           [--mem 256] [--steps N] [--log-illegal] [--lockstep qemu-cpu.log]
//!
//! Boots like `qemu-system-riscv64 -M virt -bios ... -kernel ...`. The serial
//! console (16550) is on stdin/stdout; Ctrl-A x quits.
//!
//! With --lockstep, the state before every instruction is compared against a
//! trace from `qemu-system-riscv64 ... -accel tcg,one-insn-per-tb=on
//! -d cpu,int,exec,nochain` (see tools/rv64-ref/lockstep.sh).

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::process::{exit, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use v86_rv64::boot;
use v86_rv64::bus::Bus;
use v86_rv64::cpu::{Cpu, RunExit};
use v86_rv64::dev::rtc::Rtc;
use v86_rv64::dev::uart::Uart;
use v86_rv64::dev::virtio::{Empty, VirtioDevice, VirtioMmio};
use v86_rv64::dev::virtio_blk::VirtioBlk;

struct Args {
    bios: String,
    kernel: String,
    dtb: String,
    disk: Option<String>,
    mem_mb: u64,
    steps: u64,
    log_illegal: bool,
    lockstep: Option<String>,
}

fn usage() -> ! {
    eprintln!("usage: rv64run --bios fw_dynamic.bin --kernel Image --dtb virt.dtb [--disk rootfs] [--mem MB] [--steps N] [--log-illegal] [--lockstep qemu.log]");
    exit(2)
}

fn parse_args() -> Args {
    let mut a = Args {
        bios: String::new(),
        kernel: String::new(),
        dtb: String::new(),
        disk: None,
        mem_mb: 256,
        steps: u64::MAX,
        log_illegal: false,
        lockstep: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| usage());
        match k.as_str() {
            "--bios" => a.bios = val(),
            "--kernel" => a.kernel = val(),
            "--dtb" => a.dtb = val(),
            "--disk" => a.disk = Some(val()),
            "--mem" => a.mem_mb = val().parse().unwrap_or_else(|_| usage()),
            "--steps" => a.steps = val().parse().unwrap_or_else(|_| usage()),
            "--log-illegal" => a.log_illegal = true,
            "--lockstep" => a.lockstep = Some(val()),
            _ => usage(),
        }
    }
    if a.bios.is_empty() || a.kernel.is_empty() || a.dtb.is_empty() {
        usage();
    }
    a
}

struct RawTerm(bool);

impl RawTerm {
    fn enable() -> RawTerm {
        if !std::io::stdin().is_terminal() {
            return RawTerm(false);
        }
        let ok = Command::new("stty").args(["raw", "-echo"]).stdin(std::process::Stdio::inherit()).status();
        RawTerm(ok.map(|s| s.success()).unwrap_or(false))
    }
}

impl Drop for RawTerm {
    fn drop(&mut self) {
        if self.0 {
            let _ = Command::new("stty").arg("sane").stdin(std::process::Stdio::inherit()).status();
        }
    }
}

fn read(path: &str) -> Vec<u8> { std::fs::read(path).unwrap_or_else(|e| panic!("{path}: {e}")) }

fn main() {
    let args = parse_args();
    let mut bus = Bus::new((args.mem_mb << 20) as usize);
    let out = |b: u8| {
        let _ = std::io::stdout().write_all(&[b]);
    };
    let uart = Uart::new(10, Box::new(out));
    let rx = uart.rx.clone();
    bus.map(0x1000_0000, 0x100, Box::new(uart));
    let epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    bus.map(0x10_1000, 0x1000, Box::new(Rtc::new(epoch)));
    let disk: Box<dyn VirtioDevice> = match &args.disk {
        Some(p) => Box::new(VirtioBlk::new(read(p))),
        None => Box::new(Empty),
    };
    bus.map(0x1000_1000, 0x1000, Box::new(VirtioMmio::new(1, disk)));
    bus.map(0x1000_2000, 0x1000, Box::new(VirtioMmio::new(2, Box::new(Empty))));

    let mut cpu = Cpu::new(bus);
    cpu.ns_per_insn = 1;
    cpu.log_illegal = args.log_illegal;
    let info = boot::load(&mut cpu, &read(&args.bios), &read(&args.kernel), &read(&args.dtb)).unwrap_or_else(|e| {
        eprintln!("boot: {e}");
        exit(1)
    });
    eprintln!(
        "rv64run: firmware at {:#x}, kernel at {:#x}, dtb at {:#x}, {} MB RAM",
        info.firmware, info.kernel, info.dtb, args.mem_mb
    );
    match &args.lockstep {
        Some(path) => lockstep(&mut cpu, path, args.steps),
        None => interactive(&mut cpu, rx, args.steps),
    }
}

fn interactive(cpu: &mut Cpu, rx: Arc<Mutex<VecDeque<u8>>>, steps: u64) {
    let quit = Arc::new(AtomicBool::new(false));
    let _term = RawTerm::enable();
    {
        let quit = quit.clone();
        std::thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 64];
            let mut ctrl_a = false;
            loop {
                let n = match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(n) => n,
                };
                if std::env::var_os("RV64_UART_TRACE").is_some() {
                    eprintln!("[stdin] {:?}", String::from_utf8_lossy(&buf[..n]));
                }
                let mut q = rx.lock().unwrap();
                for &b in &buf[..n] {
                    if ctrl_a {
                        ctrl_a = false;
                        if b == b'x' {
                            quit.store(true, Ordering::Relaxed);
                            return;
                        }
                        if b != 1 {
                            q.push_back(1);
                        }
                    } else if b == 1 {
                        ctrl_a = true;
                        continue;
                    }
                    q.push_back(b);
                }
            }
        });
    }

    let start = Instant::now();
    let mut done = 0u64;
    let exit_reason = loop {
        if quit.load(Ordering::Relaxed) {
            break "quit";
        }
        if done >= steps {
            break "step limit";
        }
        let n = 200_000.min(steps - done);
        let r = cpu.run(n);
        done += n;
        cpu.bus.now_ns = cpu.now_ns();
        cpu.bus.poll_devices();
        cpu.update_mip();
        let _ = std::io::stdout().flush();
        match r {
            RunExit::Budget => {}
            RunExit::Idle => std::thread::sleep(std::time::Duration::from_millis(5)),
            RunExit::PowerOff => break "power off",
            RunExit::Reset => break "reset",
        }
    };
    let _ = std::io::stdout().flush();
    drop(_term);
    let secs = start.elapsed().as_secs_f64();
    eprintln!(
        "\r\nrv64run: {exit_reason}; {} instructions in {:.2}s ({:.1} MIPS), pc={:#x} prv={}",
        cpu.icount,
        secs,
        cpu.icount as f64 / secs / 1e6,
        cpu.pc,
        cpu.prv
    );
    eprintln!(
        "rv64run: mip {:#x} mie {:#x} mideleg {:#x} mstatus {:#x} waiting {} mtime {} mtimecmp {} next_timer_ns {}",
        cpu.csr.mip,
        cpu.csr.mie,
        cpu.csr.mideleg,
        cpu.csr.mstatus,
        cpu.waiting,
        cpu.bus.mtime(),
        cpu.bus.clint.mtimecmp,
        cpu.next_timer_ns
    );
    let p = &cpu.bus.plic;
    for i in 1..12 {
        if p.level[i] || p.pending[i] || p.claimed[i] || p.enable[1][i] {
            eprintln!(
                "rv64run: plic source {i}: level {} pending {} claimed {} enable {:?} priority {}",
                p.level[i], p.pending[i], p.claimed[i], [p.enable[0][i], p.enable[1][i]], p.priority[i]
            );
        }
    }
    eprintln!("rv64run: plic threshold {:?} out {:?} seip_sw {}", p.threshold, p.out, cpu.csr.seip_sw);
    eprintln!("rv64run: interrupts taken {:?}", cpu.trap_counts[0]);
    eprintln!("rv64run: exceptions taken {:?}", cpu.trap_counts[1]);
}

// ---- lockstep ----

/// One instruction's state from QEMU's log.
struct Record {
    pc: u64,
    vals: Vec<(String, u64)>,
    text: String,
    /// An asynchronous trap (interrupt) was logged before this record.
    irq: Option<u64>,
    /// QEMU logged "Stopped execution of TB chain" before this record: the
    /// previous record's instruction didn't run.
    prev_stopped: bool,
}

fn read_record(rdr: &mut impl BufRead) -> Option<Record> {
    let mut rec = Record { pc: 0, vals: Vec::new(), text: String::new(), irq: None, prev_stopped: false };
    let mut line = String::new();
    let mut started = false;
    loop {
        line.clear();
        if rdr.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if line.starts_with("Stopped execution") {
            rec.prev_stopped = true;
            continue;
        }
        if let Some(rest) = line.strip_prefix("riscv_cpu_do_interrupt:") {
            if rest.contains("async:1") {
                let c = rest.split("cause:").nth(1).and_then(|c| u64::from_str_radix(&c[..16], 16).ok());
                rec.irq = c.map(|c| c & !(1 << 63));
            }
            continue;
        }
        if line.starts_with("Trace") {
            started = true;
            rec.text.clear();
            continue;
        }
        if !started || !line.starts_with(' ') {
            continue;
        }
        rec.text.push_str(&line);
        let mut it = line.split_whitespace();
        while let (Some(k), Some(v)) = (it.next(), it.next()) {
            let Ok(v) = u64::from_str_radix(v, 16) else { break };
            // "x10/a0" -> "x10"
            let k = k.split('/').next().unwrap();
            if k == "pc" {
                rec.pc = v;
            }
            rec.vals.push((k.to_string(), v));
        }
        if line.starts_with(" x28") {
            return Some(rec);
        }
    }
}

/// CSRs whose value depends on time or instruction counts, which may differ.
fn reads_counter(insn: u32) -> bool {
    insn & 0x7f == 0x73 && insn >> 12 & 7 != 0 && matches!(insn >> 20, 0xc00..=0xc02 | 0xb00 | 0xb02)
}

/// Feature-probe CSRs where QEMU's answer may differ from ours (debug
/// triggers); after an access, adopt QEMU's state (pc, x, M-mode trap CSRs).
fn probes_optional(insn: u32) -> bool { insn & 0x7f == 0x73 && insn >> 12 & 7 != 0 && matches!(insn >> 20, 0x7a0..=0x7a5) }

fn lockstep(cpu: &mut Cpu, path: &str, steps: u64) {
    let f = File::open(path).unwrap_or_else(|e| panic!("{path}: {e}"));
    let mut rdr = BufReader::with_capacity(1 << 20, f);
    let mut recent: VecDeque<u64> = VecDeque::with_capacity(64);
    let mut n = 0u64;
    let start = Instant::now();
    cpu.auto_irq = false;
    let Some(mut cur) = read_record(&mut rdr) else {
        eprintln!("lockstep: empty QEMU trace");
        return;
    };
    let mut prev_text = String::new();
    let mut last_insn = 0u32;
    loop {
        if n >= steps {
            eprintln!("lockstep: {n} instructions matched (step limit)");
            return;
        }
        if let Some(x) = cpu.housekeeping() {
            eprintln!("lockstep: stopped ({x:?}) after {n} instructions");
            return;
        }
        // QEMU logs nothing for an instruction whose fetch faults.
        if cpu.fetch(cpu.pc).is_err() && cur.pc != cpu.pc {
            cpu.step();
            continue;
        }
        compare(cpu, &cur, &prev_text, &recent, n, last_insn);
        let Some(next) = read_record(&mut rdr) else {
            eprintln!("lockstep: QEMU trace ended; {n} instructions matched");
            return;
        };
        if !next.prev_stopped {
            if recent.len() == 64 {
                recent.pop_front();
            }
            recent.push_back(cpu.pc);
            last_insn = cpu.fetch(cpu.pc).map_or(0, |(i, _)| i);
            cpu.bus.last_mmio_read = None;
            cpu.step();
            n += 1;
            if n % 10_000_000 == 0 {
                eprintln!("lockstep: {n} matched ({:.1}s)", start.elapsed().as_secs_f64());
            }
        }
        // Interrupt timing depends on QEMU's host scheduling; take them where it did.
        if let Some(irq) = next.irq {
            cpu.update_mip();
            if cpu.pending_interrupt() != Some(irq) {
                eprintln!(
                    "lockstep: QEMU took interrupt {irq} before #{n}; ours pending {:?} (mip {:#x} mie {:#x})",
                    cpu.pending_interrupt(),
                    cpu.csr.mip,
                    cpu.csr.mie
                );
            }
            cpu.waiting = false;
            let pc = cpu.pc;
            cpu.trap(irq, 0, true, pc);
        }
        prev_text = std::mem::replace(&mut cur, next).text;
    }
}

fn compare(cpu: &mut Cpu, rec: &Record, prev_text: &str, recent: &VecDeque<u64>, n: u64, last_insn: u32) {
    let get = |k: &str| rec.vals.iter().find(|(name, _)| name == k).map(|&(_, v)| v);
    // Device models and clocks may differ from QEMU's: after a device or
    // counter read, adopt QEMU's general registers.
    let mmio = cpu.bus.last_mmio_read;
    if (mmio.is_some() || reads_counter(last_insn)) && rec.pc == cpu.pc {
        for r in 1..32 {
            if let Some(q) = get(&format!("x{r}")) {
                if q != cpu.x[r] {
                    // (rdtime reads one tick ahead of QEMU: RV64_LOG_FOLLOW shows those)
                    if mmio.is_some() || std::env::var_os("RV64_LOG_FOLLOW").is_some() {
                        eprintln!("lockstep: #{n} follow {mmio:x?} insn {last_insn:08x} -> x{r}={q:#x} (ours {:#x})", cpu.x[r]);
                    }
                    cpu.x[r] = q;
                }
            }
        }
    }
    // Linux probes for RV32 user mode by writing UXL; QEMU has it, we don't.
    let uxl = |v: u64| v >> 32 & 3;
    let writes_status = last_insn & 0x7f == 0x73 && last_insn >> 12 & 7 != 0 && matches!(last_insn >> 20, 0x100 | 0x300);
    let uxl_probe = writes_status && get("mstatus").is_some_and(|q| uxl(q) != uxl(cpu.csr.mstatus));
    if uxl_probe || probes_optional(last_insn) {
        let trapped = get("mcause") != Some(cpu.csr.mcause) || get("mepc") != Some(cpu.csr.mepc);
        for (k, v) in &rec.vals {
            let c = &mut cpu.csr;
            match k.as_str() {
                "pc" => cpu.pc = *v,
                "mstatus" => c.mstatus = v & !(1 << 63),
                "mepc" => c.mepc = *v,
                "mcause" => c.mcause = *v,
                "mtval" => c.mtval = *v,
                _ => {
                    if let Some(r) = k.strip_prefix('x').and_then(|r| r.parse::<usize>().ok()) {
                        if r != 0 {
                            cpu.x[r] = *v;
                        }
                    }
                }
            }
        }
        if trapped {
            eprintln!("lockstep: #{n} insn {last_insn:08x} trapped in QEMU; following");
            cpu.prv = 3;
        }
    }
    let c = &cpu.csr;
    let fs = 3u64 << 13;
    let mstatus = if c.mstatus & fs == fs { c.mstatus | 1 << 63 } else { c.mstatus };
    let mut ours: Vec<(String, u64)> = vec![
        ("pc".into(), cpu.pc),
        ("mstatus".into(), mstatus),
        ("mideleg".into(), c.mideleg),
        ("medeleg".into(), c.medeleg),
        ("mie".into(), c.mie),
        ("mtvec".into(), c.mtvec),
        ("stvec".into(), c.stvec),
        ("mepc".into(), c.mepc),
        ("sepc".into(), c.sepc),
        ("mcause".into(), c.mcause),
        ("scause".into(), c.scause),
        ("mtval".into(), c.mtval),
        ("stval".into(), c.stval),
        ("mscratch".into(), c.mscratch),
        ("sscratch".into(), c.sscratch),
        ("satp".into(), c.satp),
    ];
    for r in 1..32 {
        ours.push((format!("x{r}"), cpu.x[r]));
    }
    let diffs: Vec<String> = ours
        .iter()
        .filter_map(|(k, v)| get(k).filter(|q| q != v).map(|q| format!("{k}: qemu={q:#x} ours={v:#x}")))
        .collect();
    if diffs.is_empty() {
        return;
    }
    eprintln!("lockstep: divergence before instruction #{n} (prv {})", cpu.prv);
    eprintln!("recent PCs: {}", recent.iter().map(|p| format!("{p:#x}")).collect::<Vec<_>>().join(" "));
    for d in &diffs {
        eprintln!("  {d}");
    }
    eprintln!("last executed insn: {last_insn:08x}; mip {:#x}", cpu.csr.mip);
    eprintln!("QEMU previous record:\n{prev_text}");
    eprintln!("QEMU record:\n{}", rec.text);
    exit(1);
}
