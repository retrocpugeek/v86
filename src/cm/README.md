# v86-cm: Cortex-M core (work in progress)

An ARMv7-M / ARMv7E-M (Cortex-M4, Thumb-2 with the DSP instructions, no
FPU yet) system emulator core for microcontroller firmware. The first
target is PebbleOS's emulator boards: the "pebble-generic" machine of
Pebble's QEMU fork (`dev/pebble.rs`), on which PebbleOS for `qemu_flint`
(the Pebble 2 Duo's platform) boots to its menu, watchface and apps, with
its buttons. Like `src/a64` and `src/rv64`, it's a standalone crate: a
native runner (`cmrun`), and the browser build (`build/v86-cm.wasm`, run
by `src/cm_cpu.js` as `arch: "pebble-flint"`, `"pebble-emery"` or
`"pebble-gabbro"`), which the v86-arm site shows as a watch
(`?build=pebble-flint`; `tools/a64-site/pebble/build.sh`, `add-watch.sh`).

## Layout

| File | Contents |
|---|---|
| `cpu.rs` | Registers, the banked stack pointers (MSP/PSP), special registers, IT state, the run loop, exception entry and return, faults, SVC, WFI |
| `thumb.rs` | The 16- and 32-bit Thumb-2 instructions (ARMv7-M ARM chapter A5) |
| `scs.rs` | The System Control Space: NVIC, SCB, SysTick, the MPU's registers (not enforced), DWT's cycle counter |
| `bus.rs` | The memory map: flash, SRAM, bit-band aliases, external flash (XIP), devices with interrupt lines |
| `elf.rs` | Loading firmware from an ELF file (its segments at their load addresses) |
| `dev/pebble.rs` | Pebble's emulator devices: UARTs, timers, RTC, buttons, system control, display (1-bit and 8-bit, round), external flash, audio, touch (registers only), the CMSDK watchdog |
| `wasm.rs` | The browser build's interface (`src/cm_cpu.js` drives it in real time) |
| `bin/cmrun.rs` | Native runner |

## Running

PebbleOS is built for `qemu_flint` with its own tooling (see the Pebble
emulation notes; it runs in a container). Then:

```sh
cargo build --release
target/release/cmrun --board flint --elf pebbleos.elf --spi qemu_spi_flash.bin \
    --seconds 26 --keys back@20 --screenshot watchface.ppm
```

The firmware's log (Pebble's binary framing, PULSE, on UART2) goes to
standard output.

In node and the browser: `tests/cm/pebble.mjs` (the V86 API) and
`tests/cm/pebble-page.mjs` (the site's watch page).

## Checking against QEMU

`tools/cm-ref/lockstep.sh` runs Pebble's QEMU with a fixed 16 ns per
instruction and its register dumps, runs `cmrun` the same way, and prints
the first instruction where the registers differ. Values from the host's
clock differ by nature (the RTC, and QEMU's virtual clock runs slightly
ahead of the instruction count), so a first difference at an RTC read is
expected.

Debugging switches of `cmrun`: `CM_PCTRACE=N` (trace N instructions; with
`CM_REGS`, their registers), `CM_NS16` (16 ns an instruction, as QEMU's
`-icount shift=4`), `CM_EPOCH` (the RTC's start, in Unix seconds),
`CM_SAMPLE=S` (sample the PC from S seconds on), `CM_DUMP` (the exception
state at the end).
