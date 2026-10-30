//! Native runner for the Cortex-M core: boots PebbleOS on an emulator board
//! and shows its serial console; for development and comparing with QEMU.
//!
//!   cmrun --board flint --elf pebbleos.elf --spi qemu_spi_flash.bin \
//!         [--seconds 20] [--screenshot out.ppm] [--keys 'select@8,back@12'] [--trace N]
//!
//! --trace N prints the first N instructions' PC and registers (for
//! lockstep comparison with QEMU's -d exec,cpu log).

use std::io::Write;

use v86_cm::dev::pebble::{self, Board, Gpio, Uart, GPIO_BASE, UART_BASE};

fn usage() -> ! {
    eprintln!("usage: cmrun --board flint|emery|gabbro --elf FILE [--spi FILE] [--seconds S] [--screenshot FILE.ppm] [--keys back|up|select|down@S,...] [--trace N]");
    std::process::exit(2)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut board, mut elf, mut spi, mut seconds, mut shot, mut keys, mut trace) =
        (Board::Flint, None, None, 10.0f64, None, Vec::new(), 0u64);
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| usage());
        match a.as_str() {
            "--board" => {
                board = match val().as_str() {
                    "flint" => Board::Flint,
                    "emery" => Board::Emery,
                    "gabbro" => Board::Gabbro,
                    _ => usage(),
                }
            }
            "--elf" => elf = Some(val()),
            "--spi" => spi = Some(val()),
            "--seconds" => seconds = val().parse().unwrap_or_else(|_| usage()),
            "--screenshot" => shot = Some(val()),
            "--keys" => {
                for k in val().split(',') {
                    let (name, at) = k.split_once('@').unwrap_or_else(|| usage());
                    let b = match name {
                        "back" => pebble::BTN_BACK,
                        "up" => pebble::BTN_UP,
                        "select" => pebble::BTN_SELECT,
                        "down" => pebble::BTN_DOWN,
                        _ => usage(),
                    };
                    keys.push((at.parse::<f64>().unwrap_or_else(|_| usage()), b));
                }
            }
            "--trace" => trace = val().parse().unwrap_or_else(|_| usage()),
            _ => usage(),
        }
    }
    let elf = std::fs::read(elf.unwrap_or_else(|| usage())).expect("reading the ELF");
    let spi = spi.map(|f| std::fs::read(f).expect("reading the SPI flash image")).unwrap_or_default();
    let epoch = std::env::var("CM_EPOCH").ok().and_then(|v| v.parse().ok()).unwrap_or_else(|| {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64
    });
    let mut cpu = pebble::machine(board, &elf, &spi, epoch).unwrap_or_else(|e| {
        eprintln!("cmrun: {e}");
        std::process::exit(1)
    });

    if std::env::var("CM_NS16").is_ok() {
        // As QEMU's -icount shift=4, for lockstep comparison
        cpu.ns_per_insn_x1000 = 16_000;
    }
    if let Ok(n) = std::env::var("CM_PCTRACE") {
        // The PC of each instruction executed (and the exceptions taken), as
        // QEMU's -d exec,nochain with one instruction per block lists them
        let n: u64 = n.parse().unwrap_or(1_000_000);
        let mut out = std::io::BufWriter::new(std::io::stdout());
        let mut ipsr = cpu.ipsr;
        while cpu.icount < n {
            cpu.service();
            let pc = cpu.pc;
            if std::env::var("CM_REGS").is_ok() {
                let r = &cpu.r;
                let regs: Vec<String> = (0..15).map(|i| format!("{:08x}", if i == 13 { cpu.sp() } else { r[i] })).collect();
                writeln!(out, "{pc:08x} {} {:x}", regs.join(" "), cpu.xpsr() >> 28).unwrap();
            } else {
                writeln!(out, "{pc:08x}").unwrap();
            }
            let s = cpu.run(1);
            if cpu.ipsr != ipsr && cpu.pc != pc {
                ipsr = cpu.ipsr;
            }
            if s == v86_cm::cpu::Stop::Idle && !cpu.skip_idle() {
                cpu.warp_ns += 1_000_000;
            }
        }
        return;
    }
    for _ in 0..trace {
        let r = &cpu.r;
        println!(
            "pc={:08x} r0={:08x} r1={:08x} r2={:08x} r3={:08x} r4={:08x} r5={:08x} r6={:08x} r7={:08x} r8={:08x} r9={:08x} r10={:08x} r11={:08x} r12={:08x} sp={:08x} lr={:08x} psr={:08x}",
            cpu.pc, r[0], r[1], r[2], r[3], r[4], r[5], r[6], r[7], r[8], r[9], r[10], r[11], r[12], cpu.sp(), r[14], cpu.xpsr()
        );
        cpu.run(1);
    }

    let start = std::time::Instant::now();
    let end_ns = (seconds * 1e9) as u64;
    let mut stdout = std::io::stdout();
    while cpu.now_ns() < end_ns {
        let now_s = cpu.now_ns() as f64 / 1e9;
        while let Some(&(at, b)) = keys.first() {
            if at > now_s {
                break;
            }
            keys.remove(0);
            // A press, released 100 ms later (as a pending release)
            cpu.bus.device::<Gpio>(GPIO_BASE).unwrap().set_buttons(b);
            cpu.bus.sync_all_irqs();
            cpu.run(6_400_000);
            cpu.bus.device::<Gpio>(GPIO_BASE).unwrap().set_buttons(0);
            cpu.bus.sync_all_irqs();
        }
        let sample_from: f64 = std::env::var("CM_SAMPLE").ok().and_then(|v| v.parse().ok()).unwrap_or(f64::MAX);
        let stop = if cpu.now_ns() as f64 / 1e9 >= sample_from {
            let mut s = v86_cm::cpu::Stop::Budget;
            for _ in 0..100 {
                s = cpu.run(10_000);
                eprintln!("sample {:.4} s pc={:#010x} ipsr={} lr={:#010x}", cpu.now_ns() as f64 / 1e9, cpu.pc, cpu.ipsr, cpu.r[14]);
                if s != v86_cm::cpu::Stop::Budget {
                    break;
                }
            }
            s
        } else {
            cpu.run(1_000_000)
        };
        for (i, base) in UART_BASE.iter().enumerate() {
            let out = std::mem::take(&mut cpu.bus.device::<Uart>(*base).unwrap().tx);
            if !out.is_empty() && i != 1 {
                stdout.write_all(&out).unwrap();
            }
        }
        stdout.flush().unwrap();
        if pebble::watchdog_reset(&mut cpu) {
            eprintln!("cmrun: watchdog reset at {:.3} s", cpu.now_ns() as f64 / 1e9);
            cpu.reset();
        }
        match stop {
            v86_cm::cpu::Stop::Budget | v86_cm::cpu::Stop::Idle => {}
            v86_cm::cpu::Stop::Reset => {
                eprintln!("cmrun: reset requested at {:.3} s", cpu.now_ns() as f64 / 1e9);
                cpu.reset();
            }
            s => {
                eprintln!("cmrun: stopped ({s:?}) at {:#010x}", cpu.pc);
                break;
            }
        }
        if stop == v86_cm::cpu::Stop::Idle && !cpu.skip_idle() {
            // Nothing scheduled at all: skip ahead
            cpu.warp_ns += 1_000_000;
        }
    }
    if std::env::var("CM_DUMP").is_ok() {
        let scs = &cpu.bus.scs;
        eprintln!("pc={:#010x} lr={:#010x} sp={:#010x} ipsr={} primask={} basepri={:#x} control={:#x} sleeping={}", cpu.pc, cpu.r[14], cpu.sp(), cpu.ipsr, cpu.primask, cpu.basepri, cpu.control, cpu.sleeping);
        eprintln!("enabled={:?}", (0..32).filter(|&i| scs.enabled[i]).collect::<Vec<_>>());
        eprintln!("pending={:?} active={:?}", (1..48).filter(|&i| scs.pending[i]).collect::<Vec<_>>(), (1..48).filter(|&i| scs.active[i]).collect::<Vec<_>>());
        eprintln!("systick ctrl={:#x} load={} prio={:?}", scs.stk_ctrl, scs.stk_load, &scs.prio[4..32]);
    }
    let wall = start.elapsed().as_secs_f64();
    eprintln!(
        "cmrun: {:.2} s of machine time in {:.2} s, {} M instructions ({:.0} MIPS)",
        cpu.now_ns() as f64 / 1e9,
        wall,
        cpu.icount / 1_000_000,
        cpu.icount as f64 / 1e6 / wall
    );
    if let Some(f) = shot {
        let d = pebble::display(&mut cpu);
        let d = d.borrow();
        let (w, h) = (d.width as usize, d.height as usize);
        let mut px = vec![0u32; w * h];
        d.render(&mut px);
        let mut out = format!("P6 {w} {h} 255\n").into_bytes();
        for p in px {
            out.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]);
        }
        std::fs::write(&f, out).expect("writing the screenshot");
        eprintln!("cmrun: screenshot in {f} (display {})", if d.enabled() { "on" } else { "off" });
    }
}
