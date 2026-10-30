//! Native runner for the card10 (dev/max32665.rs): boots Epicardium and
//! Pycardium and shows the serial console (UART0); for development.
//!
//!   card10run --epicardium epicardium.elf [--pycardium pycardium.elf] [--flash fat.img] [--seconds S]
//!             [--screenshot out.ppm] [--keys 'rb@5,lt@8,...']
//!
//! --type 'S:text;...' types into the serial console (UART0) at S seconds
//! (escapes: \\n, \\r, \\xHH).
//!
//! --keys presses buttons for 200 ms at the times (s) given: lb, rb, rt
//! (left bottom, right bottom, right top) or lt (left top, the power
//! button).
//!
//! CARD10_REGS=1 reports each peripheral register the firmware touches that
//! nothing models (its first access); CARD10_DUMP=1 prints both cores'
//! state at the end; CARD10_I2C=1 logs I2C transfers; CARD10_SAMPLE=1
//! prints core 0's pc every 10,000 instructions (where it spends its time);
//! CARD10_PEEK=addr,... prints words of memory at the end.

use std::io::Write;

use v86_cm::cpu::Stop;
use v86_cm::dev::max32665::{self, Uart, UART_BASE};

fn usage() -> ! {
    eprintln!("usage: card10run --epicardium FILE [--pycardium FILE] [--flash FILE] [--seconds S]");
    std::process::exit(2)
}

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut epi, mut py, mut flash, mut seconds, mut shot) = (None, None, None, 10.0f64, None::<String>);
    let mut keys: Vec<(f64, u8)> = Vec::new();
    let mut typed: Vec<(f64, Vec<u8>)> = Vec::new();
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| usage());
        match a.as_str() {
            "--epicardium" => epi = Some(val()),
            "--pycardium" => py = Some(val()),
            "--flash" => flash = Some(val()),
            "--screenshot" => shot = Some(val()),
            "--type" => {
                // S:text, ... (\\n, \\r and \\xHH escapes)
                for t in val().split(';') {
                    let (at, text) = t.split_once(':').unwrap_or_else(|| usage());
                    let mut bytes = Vec::new();
                    let mut chars = text.chars().peekable();
                    while let Some(c) = chars.next() {
                        if c == '\\' {
                            match chars.next() {
                                Some('n') => bytes.push(b'\n'),
                                Some('r') => bytes.push(b'\r'),
                                Some('x') => {
                                    let h: String = chars.by_ref().take(2).collect();
                                    bytes.push(u8::from_str_radix(&h, 16).unwrap_or_else(|_| usage()));
                                }
                                _ => usage(),
                            }
                        } else {
                            bytes.extend(c.to_string().bytes());
                        }
                    }
                    typed.push((at.parse().unwrap_or_else(|_| usage()), bytes));
                }
            }
            "--keys" => {
                for k in val().split(',') {
                    let (name, at) = k.split_once('@').unwrap_or_else(|| usage());
                    let b = match name {
                        "lb" => 1,
                        "rb" => 2,
                        "rt" => 4,
                        "lt" => 8,
                        _ => usage(),
                    };
                    keys.push((at.parse().unwrap_or_else(|_| usage()), b));
                }
            }
            "--seconds" => seconds = val().parse().unwrap_or_else(|_| usage()),
            _ => usage(),
        }
    }
    let epi = std::fs::read(epi.unwrap_or_else(|| usage())).expect("reading Epicardium");
    let py = py.map(|f| std::fs::read(f).expect("reading Pycardium")).unwrap_or_default();
    let flash = flash.map(|f| std::fs::read(f).expect("reading the flash image")).unwrap_or_default();
    let epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let mut m = max32665::machine(&epi, &py, &flash, epoch).unwrap_or_else(|e| {
        eprintln!("card10run: {e}");
        std::process::exit(1)
    });
    max32665::log_registers(&mut m, std::env::var("CARD10_REGS").is_ok());
    max32665::log_i2c(&mut m, std::env::var("CARD10_I2C").is_ok());
    let start = std::time::Instant::now();
    let end_ns = (seconds * 1e9) as u64;
    let mut stdout = std::io::stdout();
    let mut release: Option<f64> = None;
    while m.now_ns() < end_ns {
        let now_s = m.now_ns() as f64 / 1e9;
        if release.is_some_and(|t| now_s >= t) {
            m.set_buttons(0);
            release = None;
        }
        if let Some(&(at, b)) = keys.first() {
            if now_s >= at {
                keys.remove(0);
                m.set_buttons(b);
                release = Some(now_s + 0.2);
            }
        }
        if let Some((at, _)) = typed.first() {
            if now_s >= *at {
                let (_, bytes) = typed.remove(0);
                m.bus().device::<Uart>(UART_BASE[0]).unwrap().input(&bytes);
                m.input_changed();
            }
        }
        let stop = if std::env::var("CARD10_SAMPLE").is_ok() {
            let mut st = Stop::Budget;
            for _ in 0..100 {
                st = m.run(10_000);
                let c = &m.cores[0];
                eprintln!("sample {:#010x} {} {}", c.pc, c.sleeping as u8, c.ipsr);
                if st != Stop::Budget {
                    break;
                }
            }
            st
        } else {
            m.run(1_000_000)
        };
        for b in UART_BASE {
            let out = std::mem::take(&mut m.bus().device::<Uart>(b).unwrap().tx);
            stdout.write_all(&out).unwrap();
        }
        stdout.flush().unwrap();
        match stop {
            Stop::Budget => {}
            Stop::Idle => {
                // nothing scheduled at all: skip ahead
                for c in m.cores.iter_mut() {
                    c.warp_ns += 1_000_000;
                }
            }
            s => {
                eprintln!("card10run: stopped ({s:?}) at {:.3} s", m.now_ns() as f64 / 1e9);
                break;
            }
        }
    }
    if let Ok(a) = std::env::var("CARD10_PEEK") {
        // words in memory: CARD10_PEEK=0x2000abcd,...
        for a in a.split(',') {
            let a = u32::from_str_radix(a.trim_start_matches("0x"), 16).expect("an address");
            eprintln!("[{a:#010x}] = {}", m.bus().read(a, 4));
        }
    }
    if std::env::var("CARD10_DUMP").is_ok() {
        for (i, c) in m.cores.iter().enumerate() {
            eprintln!(
                "core {i}: pc={:#010x} lr={:#010x} sp={:#010x} ipsr={} primask={} sleeping={} icount={}",
                c.pc, c.r[14], c.sp(), c.ipsr, c.primask, c.sleeping, c.icount
            );
        }
    }
    if let Some(f) = shot {
        use v86_cm::dev::st7735::{VIEW_HEIGHT, VIEW_WIDTH};
        let mut px = vec![0u32; VIEW_WIDTH * VIEW_HEIGHT];
        max32665::display(&mut m).render(&mut px);
        let mut out = format!("P6 {VIEW_WIDTH} {VIEW_HEIGHT} 255\n").into_bytes();
        for p in px {
            out.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]);
        }
        std::fs::write(&f, out).expect("writing the screenshot");
    }
    let wall = start.elapsed().as_secs_f64();
    let insns = m.cores[0].icount + m.cores[1].icount;
    eprintln!(
        "card10run: {:.2} s of machine time in {:.2} s, {} M instructions ({:.0} MIPS), core 1 {}",
        m.now_ns() as f64 / 1e9,
        wall,
        insns / 1_000_000,
        insns as f64 / 1e6 / wall,
        if m.core1_on { "running" } else { "off" }
    );
}
