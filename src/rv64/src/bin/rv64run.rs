//! Native runner for the RISC-V core.
//!
//!   rv64run --bios fw_dynamic.bin --kernel Image --dtb virt.dtb [--disk rootfs]
//!           [--mem 256] [--steps N] [--log-illegal] [--lockstep qemu-cpu.log] [--xlen 32]
//!           [--disk-slot N]
//!   rv64run (--ch32v003 | --ch32v307) firmware.bin [--steps N] [--script "..."]
//!           [--gdb ADDR [--gdb-wait]]
//!
//! --gdb serves a GDB remote stub (gdb.rs) on ADDR (e.g. 127.0.0.1:3333), as a
//! debug probe would: `target remote` stops the hart, and breakpoints work in
//! flash. With --gdb-wait the hart doesn't start until gdb has connected.
//!
//! --ch32v003 and --ch32v307 run a WCH CH32V microcontroller (dev/ch32.rs)
//! instead, in real time, with USART1 on stdin/stdout. With --script, it
//! runs as fast as it can through a list of events at virtual times (ms),
//! separated by ';', for the CH32V003 game console's inputs:
//! "<ms> show" prints the OLED display, "<ms> fire" and "<ms> nofire" press
//! and release the fire button (PA2 low), "<ms> pad <n>" sets the direction
//! buttons' ADC value (channel 2; 0 when none is pressed), "<ms> frame"
//! prints the display's frame counter and the pc (a hang shows as both
//! staying the same). --jitter moves virtual time on by up to 1 us between
//! batches of instructions, as following the host clock does in the browser.
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
    xlen32: bool,
    /// virtio-mmio transport of the disk (QEMU's -device virtio-blk-device
    /// takes the last one, 7)
    disk_slot: u64,
    ch32: Option<(v86_rv64::dev::ch32::Chip, String)>,
    script: Option<String>,
    jitter: bool,
    gdb: Option<String>,
    gdb_wait: bool,
}

fn usage() -> ! {
    eprintln!("usage: rv64run --bios fw_dynamic.bin --kernel Image --dtb virt.dtb [--disk rootfs] [--mem MB] [--steps N] [--log-illegal] [--lockstep qemu.log] [--xlen 32] [--disk-slot N]");
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
        xlen32: false,
        disk_slot: 0,
        ch32: None,
        script: None,
        jitter: false,
        gdb: None,
        gdb_wait: false,
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
            "--ch32v003" => a.ch32 = Some((v86_rv64::dev::ch32::Chip::V003, val())),
            "--ch32v307" => a.ch32 = Some((v86_rv64::dev::ch32::Chip::V307, val())),
            "--script" => a.script = Some(val()),
            "--jitter" => a.jitter = true,
            "--gdb" => a.gdb = Some(val()),
            "--gdb-wait" => a.gdb_wait = true,
            "--disk-slot" => a.disk_slot = val().parse().ok().filter(|&n| n < 8).unwrap_or_else(|| usage()),
            "--xlen" => {
                a.xlen32 = match val().as_str() {
                    "32" => true,
                    "64" => false,
                    _ => usage(),
                }
            }
            _ => usage(),
        }
    }
    if a.ch32.is_none() && (a.bios.is_empty() || a.kernel.is_empty() || a.dtb.is_empty()) {
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
    if let Some((chip, fw)) = &args.ch32 {
        let out = |b: u8| {
            let _ = std::io::stdout().write_all(&[b]);
        };
        let epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let mut cpu = v86_rv64::dev::ch32::machine(*chip, &read(fw), Box::new(out), epoch).unwrap_or_else(|e| {
            eprintln!("{chip:?}: {e}");
            exit(1)
        });
        if let Some(script) = &args.script {
            run_script(&mut cpu, script, args.jitter);
            return;
        }
        if let Some(addr) = &args.gdb {
            cpu.realtime = true;
            gdb_server(&mut cpu, addr, *chip, args.gdb_wait);
            return;
        }
        cpu.realtime = true;
        let rx = cpu.bus.ch32.as_ref().unwrap().rx.clone();
        interactive(&mut cpu, rx, args.steps);
        return;
    }
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
    // virtio-mmio transports (slot n: 0x1000_1000 + n * 0x1000, interrupt
    // n + 1): the disk's, and empty ones at slots 0 and 1 (the device tree's)
    let n = args.disk_slot;
    bus.map(0x1000_1000 + n * 0x1000, 0x1000, Box::new(VirtioMmio::new(n as u32 + 1, disk)));
    for k in [0, 1].into_iter().filter(|&k| k != n) {
        bus.map(0x1000_1000 + k * 0x1000, 0x1000, Box::new(VirtioMmio::new(k as u32 + 1, Box::new(Empty))));
    }

    let mut cpu = Cpu::new(bus);
    cpu.ns_per_insn = 1;
    cpu.log_illegal = args.log_illegal;
    cpu.xlen32 = args.xlen32;
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

/// The CH32V's OLED display as text, two pixel rows per line.
fn show_display(cpu: &Cpu, ms: u64) {
    use v86_rv64::dev::ssd1306::{HEIGHT, WIDTH};
    let mut px = vec![0u8; WIDTH * HEIGHT];
    cpu.bus.ch32.as_ref().unwrap().display.render(&mut px);
    println!("--- {ms} ms ---");
    for y in (0..HEIGHT).step_by(2) {
        let line: String = (0..WIDTH)
            .map(|x| match (px[y * WIDTH + x], px[(y + 1) * WIDTH + x]) {
                (0, 0) => ' ',
                (_, 0) => '▀',
                (0, _) => '▄',
                _ => '█',
            })
            .collect();
        println!("|{}|", line.trim_end());
    }
}

fn run_script(cpu: &mut Cpu, script: &str, jitter: bool) {
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let mut events: Vec<(u64, Vec<&str>)> = script
        .split(';')
        .filter_map(|e| {
            let mut w = e.split_whitespace();
            Some((w.next()?.parse().ok()?, w.collect()))
        })
        .collect();
    events.sort_by_key(|e| e.0);
    for (ms, what) in events {
        while cpu.now_ns() < ms * 1_000_000 {
            if jitter {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                cpu.warp_ns += seed % 1000;
            }
            if matches!(cpu.run(10_000), RunExit::PowerOff | RunExit::Reset | RunExit::Idle) {
                eprintln!("rv64run: the CPU stopped at {} ms", cpu.now_ns() / 1_000_000);
                return;
            }
        }
        let c = cpu.bus.ch32.as_mut().unwrap();
        match what.as_slice() {
            ["show"] => show_display(cpu, ms),
            ["frame"] => println!("{ms} ms: display frame {}, pc {:#x}", c.display.frame, cpu.pc),
            ["fire"] => c.gpio_in[0] = (1 << 2, 0),
            ["nofire"] => c.gpio_in[0] = (0, 0),
            ["pad", n] => c.adc_in[2] = n.parse().unwrap_or(0),
            _ => eprintln!("rv64run: unknown script event {what:?}"),
        }
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
        if cpu.realtime {
            cpu.sync_host_ns(start.elapsed().as_nanos() as u64);
        }
        let r = cpu.run(n);
        done += n;
        cpu.bus.now_ns = cpu.now_ns();
        cpu.bus.poll_devices();
        cpu.update_mip();
        let _ = std::io::stdout().flush();
        match r {
            RunExit::Budget => {}
            RunExit::Idle => {
                let ns = cpu.ns_until_timer().unwrap_or(5_000_000).min(5_000_000);
                std::thread::sleep(std::time::Duration::from_nanos(ns))
            }
            RunExit::PowerOff => break "power off",
            RunExit::Reset => break "reset",
            RunExit::Debug => {}
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
                // (16 hex digits for RV64, 8 for RV32)
                let c = rest.split("cause:").nth(1).and_then(|c| {
                    let hex: String = c.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
                    u64::from_str_radix(&hex, 16).ok()
                });
                rec.irq = c.map(|c| c & 0x7fff_ffff);
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
    // RV32: QEMU logs 32-bit values; our registers hold them sign-extended
    let xlen32 = cpu.xlen32;
    let reg = |v: u64| if xlen32 { v as i32 as i64 as u64 } else { v };
    let low = |v: u64| if xlen32 { v & 0xffff_ffff } else { v };
    // Device models and clocks may differ from QEMU's: after a device or
    // counter read, adopt QEMU's general registers.
    let mmio = cpu.bus.last_mmio_read;
    if (mmio.is_some() || reads_counter(last_insn)) && rec.pc == cpu.pc {
        for r in 1..32 {
            if let Some(q) = get(&format!("x{r}")) {
                if reg(q) != cpu.x[r] {
                    // (rdtime reads one tick ahead of QEMU: RV64_LOG_FOLLOW shows those)
                    if mmio.is_some() || std::env::var_os("RV64_LOG_FOLLOW").is_some() {
                        eprintln!("lockstep: #{n} follow {mmio:x?} insn {last_insn:08x} -> x{r}={q:#x} (ours {:#x})", cpu.x[r]);
                    }
                    cpu.x[r] = reg(q);
                }
            }
        }
    }
    // Linux probes for RV32 user mode by writing UXL; QEMU has it, we don't.
    let uxl = |v: u64| v >> 32 & 3;
    let writes_status = last_insn & 0x7f == 0x73 && last_insn >> 12 & 7 != 0 && matches!(last_insn >> 20, 0x100 | 0x300);
    let uxl_probe = !xlen32 && writes_status && get("mstatus").is_some_and(|q| uxl(q) != uxl(cpu.csr.mstatus));
    if uxl_probe || probes_optional(last_insn) {
        let trapped = get("mcause") != Some(cpu.csr.mcause) || get("mepc") != Some(cpu.csr.mepc);
        for (k, v) in &rec.vals {
            let c = &mut cpu.csr;
            match k.as_str() {
                "pc" => cpu.pc = *v,
                "mstatus" if xlen32 => c.mstatus = (c.mstatus & !0xffff_ffff) | (v & 0x7fff_ffff),
                "mstatus" => c.mstatus = v & !(1 << 63),
                "mepc" => c.mepc = *v,
                "mcause" => c.mcause = *v,
                "mtval" => c.mtval = *v,
                _ => {
                    if let Some(r) = k.strip_prefix('x').and_then(|r| r.parse::<usize>().ok()) {
                        if r != 0 {
                            cpu.x[r] = reg(*v);
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
    let sd = c.mstatus & fs == fs;
    let mstatus = if xlen32 {
        (c.mstatus & 0x7fff_ffff) | (sd as u64) << 31
    } else {
        c.mstatus | (sd as u64) << 63
    };
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
        ours.push((format!("x{r}"), low(cpu.x[r])));
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

/// --gdb: the machine in real time, with a GDB remote stub on a TCP port. One
/// debugger at a time; the hart runs on its own between them.
fn gdb_server(cpu: &mut Cpu, addr: &str, chip: v86_rv64::dev::ch32::Chip, wait: bool) {
    use std::net::TcpListener;
    use v86_rv64::gdb::{Gdb, Mode};
    let listener = TcpListener::bind(addr).unwrap_or_else(|e| {
        eprintln!("rv64run: --gdb {addr}: {e}");
        exit(1)
    });
    listener.set_nonblocking(true).unwrap();
    eprintln!("rv64run: gdb stub on {}", listener.local_addr().unwrap());
    let (embedded, fpu) = match chip {
        v86_rv64::dev::ch32::Chip::V003 => (true, false),
        _ => (false, true),
    };
    let start = Instant::now();
    let mut client: Option<(std::net::TcpStream, Gdb)> = None;
    let mut buf = [0u8; 4096];
    let mut waiting = wait;
    loop {
        if client.is_none() {
            if let Ok((s, peer)) = listener.accept() {
                s.set_nonblocking(true).unwrap();
                let _ = s.set_nodelay(true);
                eprintln!("rv64run: gdb connected from {peer}");
                client = Some((s, Gdb::attach(cpu, embedded, fpu)));
                waiting = false;
            }
        }
        let mut gone = false;
        if let Some((s, g)) = client.as_mut() {
            match s.read(&mut buf) {
                Ok(0) => gone = true,
                Ok(n) => g.input(cpu, &buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(_) => gone = true,
            }
            if !gone && !g.out.is_empty() {
                s.set_nonblocking(false).unwrap();
                gone = s.write_all(&g.out).is_err();
                s.set_nonblocking(true).unwrap();
                g.out.clear();
            }
        }
        if gone {
            eprintln!("rv64run: gdb disconnected");
            if let Some((_, mut g)) = client.take() {
                g.detach(cpu);
            }
        }
        let halted = waiting || client.as_ref().is_some_and(|(_, g)| g.mode == Mode::Halted);
        if halted {
            std::thread::sleep(std::time::Duration::from_millis(1));
            continue;
        }
        cpu.sync_host_ns(start.elapsed().as_nanos() as u64);
        let r = match client.as_mut() {
            Some((_, g)) => g.poll(cpu, 20_000).unwrap_or(RunExit::Budget),
            None => cpu.run(20_000),
        };
        cpu.bus.now_ns = cpu.now_ns();
        cpu.bus.poll_devices();
        cpu.update_mip();
        let _ = std::io::stdout().flush();
        match r {
            RunExit::Idle => {
                let ns = cpu.ns_until_timer().unwrap_or(1_000_000).min(1_000_000);
                std::thread::sleep(std::time::Duration::from_nanos(ns))
            }
            RunExit::PowerOff | RunExit::Reset => {
                eprintln!("rv64run: the machine stopped ({})", if matches!(r, RunExit::Reset) { "reset" } else { "power off" });
                return;
            }
            RunExit::Budget | RunExit::Debug => {
                // Real time: don't get ahead of the host clock
                let ahead = cpu.ns_ahead(start.elapsed().as_nanos() as u64);
                if ahead > 2_000_000 {
                    std::thread::sleep(std::time::Duration::from_nanos(ahead.min(5_000_000)));
                }
            }
        }
    }
}
