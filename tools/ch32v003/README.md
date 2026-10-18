# CH32V003 firmware

Bare-metal firmware for the WCH CH32V003 (QingKe V2A, RV32EC, 2 KB of SRAM,
16 KB of flash), run by v86's RISC-V core as `arch: "ch32v003"`
(`src/rv64/src/dev/ch32.rs`) and on the v86-riscv site.

`tetris.c` is Tetris over USART1 (115200 8N1, TX on PD5, RX on PD6) for an
ANSI terminal: 10 x 20 board, 7-bag randomizer, ghost piece, levels. Keys:
arrows or a/d/w/s, space hard drop, z rotate back, p pause, r restart. It
runs from a 10 ms SysTick interrupt and sleeps (`wfi`) in between, and only
redraws what changed.

No SDK: `start.S` (reset entry), `link.ld`, `ch32v003.h` (the registers
used) and `libgcc.c` (multiply, divide, memset, memcpy for RV32EC). The
same image should run on a real CH32V003 with a USB-serial adapter on
PD5/PD6 (flash it with e.g. minichlink); that hasn't been tried yet.

```sh
CROSS=riscv32-buildroot-linux-gnu- tools/ch32v003/build.sh     # any rv32ec-capable GCC
src/rv64/target/release/rv64run --ch32v003 build/ch32v003/tetris.bin
tests/rv64/ch32v003.mjs
tools/rv64-site/add-mcu.sh ch32v003-tetris "CH32V003 (bare metal): Tetris" build/ch32v003/tetris.bin
```

The emulated CH32V003 models SysTick, the PFIC, USART1 and RCC; see
`dev/ch32.rs` for what isn't (the hardware register stacking, which the
firmware turns off; timers, ADC, I2C, SPI, DMA).
