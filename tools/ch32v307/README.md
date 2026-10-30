# CH32V307 firmware: RT-Thread

`build.sh` builds [RT-Thread](https://github.com/RT-Thread/rt-thread) (its
`msh` shell on USART1, 115200 8N1) for v86's emulated WCH CH32V307
(`src/rv64/src/dev/ch32.rs`, `arch: "ch32v307"`), from RT-Thread's
`ch32v307v-r1` BSP and its CH32V307 SDK package, fetched into
`build/ch32v307/work` at a pinned commit and checksum. Output:
`build/ch32v307/rtthread.bin` (flash image) and `rtthread.json` (sizes, for
the site).

It needs git, scons and a bare-metal RISC-V GCC with picolibc; on Ubuntu:

```sh
sudo apt install gcc-riscv64-unknown-elf picolibc-riscv64-unknown-elf scons
tools/ch32v307/build.sh
src/rv64/target/release/rv64run --ch32v307 build/ch32v307/rtthread.bin   # native, Ctrl-A x quits
tests/rv64/ch32v307.mjs
```

Changes to the BSP (made by the script, in a copy):

- the Ubuntu toolchain and picolibc instead of WCH's GCC 8 and newlib;
- WCH's `interrupt("WCH-Interrupt-fast")` (handlers that leave register
  saving to the hardware) becomes GCC's standard `interrupt("machine")`;
  with the emulated hardware stacking also on, registers are just saved
  twice;
- `v86.c`: the idle thread sleeps (`wfi`) instead of spinning, so the
  emulator idles too.

Shell commands added by the demo packages (fetched at pinned commits;
`patches/` adapts them: snake and aclock read the console without POSIX
termios and quit on a key, Lua2RTT follows RT-Thread 5's thread struct):

- `snake` ([mysterywolf/snake](https://github.com/mysterywolf/snake), GPL-2.0): w/a/s/d or arrows, q quits
- `aclock` ([mysterywolf/aclock](https://github.com/mysterywolf/aclock)): an analog clock; any key quits
- `core_mark` ([wuhanstudio/coremark](https://github.com/wuhanstudio/coremark)): CoreMark, 3600 iterations; in
  the emulator, the score measures the emulator (time follows the host's clock)
- `lua` ([liu2guang/Lua2RTT](https://github.com/liu2guang/Lua2RTT)): Lua 5.1.4; Ctrl-D quits

The board's `main.c` blinks LED1 on PB5, which the v86-riscv site shows.
With the demos the image is about 240 KB, so the build uses the chip's
256 KB flash / 64 KB SRAM split, and `date` and `aclock` get the time from
the emulated RTC (in UTC).
What the emulated chip models (and doesn't) is listed in `dev/ch32.rs`.
