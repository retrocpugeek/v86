//! Native runner for the AArch64 core.
//!
//!   a64run --kernel Image --dtb v86-virt.dtb [--disk rootfs.ext4] [--mem 128] [--steps N]
//!          [--lockstep qemu-cpu.log] [--icount-shift S] [--quiet-undef]
//!   a64run --bios u-boot.bin --dtb v86-virt.dtb [--disk disk.img] ...
//!
//! The serial console (PL011) is attached to stdin/stdout. Ctrl-A x quits.
//! With --lockstep, the register state before every instruction is compared
//! against a trace from
//!   qemu-system-aarch64 -M virt -cpu cortex-a53 -accel tcg,one-insn-per-tb=on \
//!       -kernel Image -dtb <same dtb> -d cpu,nochain -D qemu-cpu.log

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::process::{exit, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use v86_a64::boot::{self, RAM_BASE};
use v86_a64::bus::Bus;
use v86_a64::cpu::{Cpu, RunExit};
use v86_a64::dev::pl011::Pl011;
use v86_a64::dev::pl031::Pl031;
use v86_a64::dev::virtio::{Empty, VirtioDevice, VirtioMmio};
use v86_a64::dev::virtio_blk::VirtioBlk;

struct Args {
    /// The kernel, or the firmware with --bios
    kernel: String,
    bios: bool,
    dtb: String,
    mem_mb: u64,
    steps: u64,
    lockstep: Option<String>,
    disk: Option<String>,
    icount_shift: u32,
    quiet_undef: bool,
}

fn usage() -> ! {
    eprintln!("usage: a64run (--kernel Image | --bios u-boot.bin) --dtb file.dtb [--disk rootfs.ext4] [--mem MB] [--steps N] [--lockstep qemu.log] [--icount-shift S] [--quiet-undef]");
    exit(2)
}

fn parse_args() -> Args {
    let mut a = Args {
        kernel: String::new(),
        bios: false,
        dtb: String::new(),
        mem_mb: 128,
        steps: u64::MAX,
        lockstep: None,
        disk: None,
        icount_shift: 0,
        quiet_undef: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| usage());
        match k.as_str() {
            "--kernel" => a.kernel = val(),
            "--bios" => {
                a.kernel = val();
                a.bios = true;
            }
            "--dtb" => a.dtb = val(),
            "--mem" => a.mem_mb = val().parse().unwrap_or_else(|_| usage()),
            "--steps" => a.steps = val().parse().unwrap_or_else(|_| usage()),
            "--lockstep" => a.lockstep = Some(val()),
            "--disk" => a.disk = Some(val()),
            "--icount-shift" => a.icount_shift = val().parse().unwrap_or_else(|_| usage()),
            "--quiet-undef" => a.quiet_undef = true,
            _ => usage(),
        }
    }
    if a.kernel.is_empty() || a.dtb.is_empty() {
        usage();
    }
    a
}

/// Restores the terminal mode on drop.
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

fn main() {
    let args = parse_args();
    let image = std::fs::read(&args.kernel).unwrap_or_else(|e| panic!("{}: {e}", args.kernel));
    let dtb = std::fs::read(&args.dtb).unwrap_or_else(|e| panic!("{}: {e}", args.dtb));

    let mut bus = Bus::new(RAM_BASE, (args.mem_mb << 20) as usize);
    let out = |b: u8| {
        let _ = std::io::stdout().write_all(&[b]);
    };
    let uart = Pl011::new(1, Box::new(out));
    let rx = uart.rx.clone();
    bus.map(0x0900_0000, 0x1000, Box::new(uart));
    let epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    bus.map(0x0901_0000, 0x1000, Box::new(Pl031::new(2, epoch)));
    // virtio-mmio slots as in v86-virt.dts; QEMU fills the highest slot first,
    // so the disk sits at 0xa003e00 (SPI 47) and becomes virtio0/vda.
    let disk: Box<dyn VirtioDevice> = match &args.disk {
        Some(p) => Box::new(VirtioBlk::new(std::fs::read(p).unwrap_or_else(|e| panic!("{p}: {e}")))),
        None => Box::new(Empty),
    };
    bus.map(0x0a00_3e00, 0x200, Box::new(VirtioMmio::new(47, disk)));
    for (slot, spi) in [(0x0a00_3c00u64, 46u32), (0x0a00_3a00, 45), (0x0a00_3800, 44)] {
        bus.map(slot, 0x200, Box::new(VirtioMmio::new(spi, Box::new(Empty))));
    }

    let mut cpu = Cpu::new(bus);
    cpu.ns_per_insn = 1 << args.icount_shift;
    cpu.log_undef = !args.quiet_undef;
    let loaded = if args.bios { boot::load_firmware(&mut cpu, &image, &dtb) } else { boot::load(&mut cpu, &image, &dtb) };
    let info = loaded.unwrap_or_else(|e| {
        eprintln!("boot: {e}");
        exit(1)
    });
    let what = if args.bios { "firmware" } else { "kernel" };
    eprintln!("a64run: {what} at {:#x}, dtb at {:#x}, {} MB RAM", info.entry, info.dtb, args.mem_mb);

    if let Some(path) = &args.lockstep {
        lockstep(&mut cpu, path, args.steps);
        return;
    }
    interactive(&mut cpu, rx, args.steps);
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
        let before = cpu.icount;
        let r = cpu.run(200_000.min(steps - done));
        done += 200_000.min(steps - done);
        cpu.bus.now_ns = cpu.now_ns();
        cpu.bus.poll_devices();
        let _ = std::io::stdout().flush();
        let _ = before;
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
        "\r\na64run: {exit_reason}; {} instructions in {:.2}s ({:.1} MIPS), pc={:#x}",
        cpu.icount,
        secs,
        cpu.icount as f64 / secs / 1e6,
        cpu.pc
    );
    if let Some(i) = cpu.first_el0 {
        eprintln!("a64run: first EL0 entry at icount {i}");
    }
    eprintln!("a64run: {} decoded code pages", cpu.dcache.pages());
}

/// One `-d cpu` record from QEMU.
struct Record {
    vals: HashMap<String, u64>,
    text: String,
    /// QEMU took an IRQ immediately before this record (from -d int).
    irq: bool,
    /// The previous record's TB was exited before executing (from -d exec).
    prev_stopped: bool,
}

thread_local! {
    /// Last raw lines read from the QEMU log, for divergence reports.
    static RAW: std::cell::RefCell<VecDeque<String>> = std::cell::RefCell::new(VecDeque::new());
}

fn read_record(r: &mut impl BufRead) -> Option<Record> {
    let mut vals = HashMap::new();
    let mut text = String::new();
    let mut line = String::new();
    let mut irq = false;
    let mut prev_stopped = false;
    loop {
        line.clear();
        if r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if !line.starts_with('X') {
            RAW.with(|raw| {
                let mut raw = raw.borrow_mut();
                if raw.len() == 40 {
                    raw.pop_front();
                }
                raw.push_back(line.clone());
            });
        }
        if vals.is_empty() && !line.trim_start().starts_with("PC=") {
            if line.starts_with("Taking exception 5 [IRQ]") {
                irq = true;
            } else if line.starts_with("Stopped execution of TB chain") {
                prev_stopped = true;
            }
            continue; // other log output
        }
        text.push_str(&line);
        for tok in line.split_whitespace() {
            if let Some((k, v)) = tok.split_once('=') {
                if let Ok(v) = u64::from_str_radix(v, 16) {
                    vals.insert(k.to_string(), v);
                }
            }
        }
        if line.starts_with("PSTATE=") {
            return Some(Record { vals, text, irq, prev_stopped });
        }
    }
}

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
    loop {
        if n >= steps {
            eprintln!("lockstep: {n} instructions matched (step limit)");
            return;
        }
        if let Some(x) = cpu.housekeeping() {
            eprintln!("lockstep: stopped ({x:?}) after {n} instructions");
            return;
        }
        // QEMU logs nothing for an instruction whose fetch faults (the TB can't
        // be translated), so take such aborts without consuming a record.
        if cpu.fetch(cpu.pc).is_err() && cur.vals.get("PC") != Some(&cpu.pc) {
            cpu.step();
            continue;
        }
        compare(cpu, &cur, &prev_text, &recent, n);
        // One record of lookahead: QEMU logs a TB before running it, and may then
        // exit it unexecuted (marked by "Stopped execution of TB chain").
        let Some(next) = read_record(&mut rdr) else {
            eprintln!("lockstep: QEMU trace ended; {n} instructions matched");
            return;
        };
        if !next.prev_stopped {
            if recent.len() == 64 {
                recent.pop_front();
            }
            recent.push_back(cpu.pc);
            cpu.bus.last_mmio_read = None;
            cpu.step();
            n += 1;
            if n % 10_000_000 == 0 {
                eprintln!("lockstep: {n} matched ({:.1}s)", start.elapsed().as_secs_f64());
            }
        }
        // Interrupt timing in QEMU depends on host scheduling (timers run on the
        // main-loop thread), so take IRQs exactly where QEMU did.
        if next.irq {
            if !cpu.bus.gic.irq_out {
                eprintln!("lockstep: QEMU took an IRQ before #{n} but none is pending here");
            }
            if std::env::var_os("A64_LOG_IRQ").is_some() {
                eprintln!("lockstep: IRQ at #{n} pending={:?}", cpu.bus.gic.pending_list());
            }
            cpu.take_irq();
        }
        prev_text = std::mem::replace(&mut cur, next).text;
    }
}

fn compare(cpu: &mut Cpu, rec: &Record, prev_text: &str, recent: &VecDeque<u64>, n: u64) {
    // Device models may differ from QEMU's: if the last instruction loaded from a
    // device register and only general registers differ, adopt QEMU's values.
    if let Some((pa, size, ours)) = cpu.bus.last_mmio_read {
        let same_pc = rec.vals.get("PC") == Some(&cpu.pc);
        let mut fixed = Vec::new();
        if same_pc {
            for r in 0..31 {
                if let Some(&q) = rec.vals.get(&format!("X{r:02}")) {
                    if q != cpu.x[r] {
                        fixed.push((r, q));
                    }
                }
            }
        }
        if same_pc && !fixed.is_empty() {
            for &(r, q) in &fixed {
                eprintln!("lockstep: #{n} mmio-follow pa={pa:#x} size={size} ours={ours:#x} -> x{r}={q:#x}");
                cpu.x[r] = q;
            }
        }
    }
    let mut diffs = Vec::new();
    let mut check = |name: &str, ours: u64| {
        if let Some(&q) = rec.vals.get(name) {
            if q != ours {
                diffs.push(format!("{name}: qemu={q:#x} ours={ours:#x}"));
            }
        }
    };
    check("PC", cpu.pc);
    for r in 0..31 {
        check(&format!("X{r:02}"), cpu.x[r]);
    }
    check("SP", cpu.sp());
    check("PSTATE", cpu.pstate());
    if diffs.is_empty() {
        return;
    }
    eprintln!("lockstep: divergence before instruction #{n}");
    let pc = cpu.pc;
    let walk = cpu.walk(pc, v86_a64::mmu::Acc::Exec).map(|w| (w.pa, w.perm, w.global, w.level));
    eprintln!(
        "  our pc={pc:#x} el={} tlb.asid={} tlb entry={:?} walk={:x?} ttbr0={:#x} tcr={:#x}",
        cpu.el,
        cpu.tlb.asid,
        cpu.tlb.probe(pc),
        walk,
        cpu.sr.ttbr0_el1,
        cpu.sr.tcr_el1
    );
    eprintln!("recent PCs: {}", recent.iter().map(|p| format!("{p:#x}")).collect::<Vec<_>>().join(" "));
    for d in &diffs {
        eprintln!("  {d}");
    }
    RAW.with(|raw| eprintln!("QEMU log tail (X lines omitted):\n{}", raw.borrow().iter().cloned().collect::<String>()));
    eprintln!("QEMU previous record:\n{prev_text}");
    eprintln!("QEMU record:\n{}", rec.text);
    if std::env::var_os("A64_DUMP_RECENT").is_some() {
        for &p in recent.iter() {
            if let Ok(insn) = cpu.fetch(p) {
                eprintln!("recent {p:#x} {insn:08x}");
            }
        }
        for r in 0..32 {
            eprintln!("  v{r} = {:032x}", cpu.v[r]);
        }
    }
    if let Some(&last) = recent.back() {
        if let Ok(insn) = cpu.fetch(last) {
            eprintln!("last executed insn: {insn:08x}");
        }
    }
    exit(1);
}
