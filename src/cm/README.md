# v86-cm: Cortex-M core (work in progress)

An ARMv7-M / ARMv7E-M and ARMv8-M Mainline (Cortex-M4 and Cortex-M33:
Thumb-2 with the DSP instructions, the single-precision FPU (FPv4-SP),
load-acquire/store-release, the stack limit registers; no Security
Extension) system emulator core for microcontroller firmware, with one
core or two. The first target is PebbleOS's emulator
boards: the "pebble-generic" machine of Pebble's QEMU fork
(`dev/pebble.rs`), on which PebbleOS for `qemu_flint` (the Pebble 2 Duo's
platform, a Cortex-M4) and `qemu_gabbro` (the Pebble Round 2's, a
Cortex-M33 with a round touch screen) boots to its menu, watchfaces and
apps, with its buttons and touch screen. Like `src/a64` and `src/rv64`, it's a standalone crate: a
native runner (`cmrun`), and the browser build (`build/v86-cm.wasm`, run
by `src/cm_cpu.js` as `arch: "pebble-flint"`, `"pebble-emery"` or
`"pebble-gabbro"`), which the v86-arm site shows as a watch
(`?build=pebble-flint`, `?build=pebble-round2`; `tools/a64-site/pebble/build.sh`,
`add-watch.sh`).

The second is the card10 badge (CCC camp 2019, `dev/max32665.rs`): a
Maxim MAX32666 with two Cortex-M4F cores, running its own firmware,
Epicardium (FreeRTOS) on core 0 and Pycardium (MicroPython) on core 1,
which calls Epicardium through shared memory, the chip's hardware
semaphores and events between the cores. It boots to its clock, menu and
apps, with its display, buttons and MicroPython's REPL on its serial port
(`card10run`; in the browser `arch: "card10"`, the site's
`?build=card10`; `tools/a64-site/card10/build.sh`, `add-card10.sh`).

## Layout

| File | Contents |
|---|---|
| `cpu.rs` | Registers, the banked stack pointers (MSP/PSP), special registers, IT state, the run loop, exception entry and return, faults, SVC, WFI |
| `thumb.rs` | The 16- and 32-bit Thumb-2 instructions (ARMv7-M ARM chapter A5) |
| `scs.rs` | The System Control Space: NVIC, SCB, SysTick, the MPU's registers (ARMv7-M's or ARMv8-M's layout; not enforced), DWT's cycle counter |
| `bus.rs` | The memory map: flash, SRAM, bit-band aliases, external flash (XIP), devices with interrupt lines |
| `elf.rs` | Loading firmware from an ELF file (its segments at their load addresses) |
| `gdb.rs` | A GDB stub per core (gdb's remote protocol, an M-profile target description with the FPU's registers): the card10's two cores each have one, and stop and step on their own |
| `fpu.rs` | The floating-point instructions (FPv4-SP; the extended exception frame is in `cpu.rs`) |
| `dev/pebble.rs` | Pebble's emulator devices: UARTs, timers, RTC, buttons, system control, display (1-bit and 8-bit, round), external flash, audio, touch, the CMSDK watchdog |
| `dev/max32665.rs` | The card10: its two cores (interleaved, each with its own SCS, sharing the bus), and the MAX32665's peripherals the firmware uses (GCR, SEMA, UART, timers and the wake-up timer, RTC, GPIO, I2C with the PMIC and port expander, SPI with the display, SPIXFC with the MX25 flash, ADC, TRNG; the rest hold what's written) |
| `dev/st7735.rs` | The card10's ST7735 display (the RISC-V core's model) |
| `wasm.rs` | The browser build's interface (`src/cm_cpu.js` drives it in real time) |
| `bin/cmrun.rs` | Native runner (Pebble) |
| `bin/card10run.rs` | Native runner (card10) |

## Running

PebbleOS is built for `qemu_flint` with its own tooling, in a container
(`tools/a64-site/pebble/build.sh` says what it needs). Then:

```sh
cargo build --release
target/release/cmrun --board flint --elf pebbleos.elf --spi qemu_spi_flash.bin \
    --seconds 26 --keys back@20 --screenshot watchface.ppm
target/release/cmrun --board gabbro ... --touch '130,220-130,60@20'   # a swipe up
```

The firmware's log (Pebble's binary framing, PULSE, on UART2) goes to
standard output.

In node and the browser: `tests/cm/pebble.mjs` (the V86 API) and
`tests/cm/pebble-page.mjs` (the site's watch page).

The card10's firmware is built in a container
(`tools/a64-site/card10/build.sh`); then:

```sh
target/release/card10run --epicardium epicardium.elf --pycardium pycardium.elf \
    --flash fat.img --seconds 10 --keys 'lt@5' --type '7:\x03;8:\x03;9:print(6*7)\r' \
    --screenshot card10.ppm
```

Its serial console (UART0: Epicardium's log, Pycardium's output) goes to
standard output; `--gdb0 127.0.0.1:3333 --gdb1 127.0.0.1:3334` serve each
core's GDB stub (`gdb-multiarch epicardium.elf -ex 'target remote :3333'`,
with the ELFs from `build/card10/debug`); `--keys` presses its buttons (lt, lb, rt, rb), `--type`
types into its console. In the browser: `tests/cm/card10-page.mjs`, and
`tests/guest-web/card10-debug.mjs` for `?build=card10&debug`, where the page
offers the stubs to a Linux machine (the gdb-wui image, booted with
`gdb_wui.card10`) that runs a gdb and a gdb-wui per core.

The browser build keeps machines to real time: one too busy for the host
(the card10's two cores) runs with its CPU clocked lower
(`cm_set_clock_scale`), its timers and clocks going on in real time.

## Checking against QEMU

`tools/cm-ref/lockstep.sh` runs Pebble's QEMU (from the PebbleOS SDK, in
the same container) with a fixed 16 ns per
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
