# v86-rv64: RISC-V core (work in progress)

An RV64GC system emulator core (RV64IMAFDC, Zicsr, Zifencei; machine,
supervisor and user mode; Sv39), modelled on QEMU's riscv64 `virt` machine.
It boots OpenSBI (`fw_dynamic`) and Linux from a Buildroot
`qemu_riscv64_virt_defconfig` image. It also runs as RV32GC (Sv32), QEMU's
riscv32 `virt`, for `qemu_riscv32_virt_defconfig` images: `Cpu::xlen32`. Like `src/a64`, it's a standalone crate:
a native runner, and the browser build (`v86-rv64.wasm`) that v86 runs as
`arch: "riscv64"` (`src/rv64_cpu.js`).

## Layout

| File | Contents |
|---|---|
| `cpu.rs` | Hart state, traps and interrupts (delegation, priorities), mret/sret, WFI, run loop |
| `csr.rs` | CSRs and their WARL rules (what OpenSBI and Linux probe) |
| `exec.rs` | RV64I, M, A, Zicsr, Zifencei, system instructions |
| `fpu.rs` | F and D: RISC-V NaN rules, all rounding modes and exception flags |
| `rvc.rs` | C: compressed instructions expanded to their 32-bit forms |
| `mmu.rs` | Sv39 walks (hardware A/D updates, as QEMU), TLB, loads/stores/fetch |
| `bus.rs` | RAM, boot ROM, CLINT, PLIC, sifive_test (power off/reset), device map |
| `boot.rs` | QEMU-identical reset vector and fw_dynamic info, kernel and DTB placement |
| `dev/` | NS16550A UART, goldfish RTC, virtio-mmio + virtio-blk (native runner) |
| `jit.rs` | Browser build: compiles hot pages of S/U-mode code to WebAssembly (see its module comment) |
| `wasm.rs` | Browser build exports; virtio devices are v86's (JS), reached through MMIO imports |

## RV32

With `xlen32`, registers hold their 32-bit values sign-extended to 64 bits,
which most RV64 instructions preserve; ADD, ADDI, the shifts, MUL and the
divisions run as their RV64 W forms (`Cpu::exec32`, and likewise in the
JIT). pc-relative results, jump targets and load/store addresses are 32
bits, CSRs read sign-extended (with the `h` halves of 64-bit ones), and
paging is Sv32. The C extension has RV32C's C.JAL, C.FLW etc. (`rvc::expand32`).

## CH32V003

`dev/ch32.rs` makes a WCH CH32V003 microcontroller out of the RV32 core:
flash at 0x0800_0000 (and 0) as ROM, 2 KB of SRAM, SysTick and the PFIC
(interrupts in machine mode), USART1 as the console, RCC. `rv64run
--ch32v003 flash.bin` runs one natively in real time; in the browser it's
`arch: "ch32v003"` with `bios` as the flash image. `tools/ch32v003` has a
Tetris for it.

## Running

```sh
cargo build --release            # from src/rv64
dtc -I dts -O dtb -o virt.dtb ../../tools/rv64-ref/virt.dts
target/release/rv64run --bios fw_dynamic.bin --kernel Image --dtb virt.dtb --disk rootfs.squashfs
```

For RV32, add `--xlen 32` and use `tools/rv64-ref/virt32.dts`. The console (16550) is on stdin/stdout; Ctrl-A x quits. `RV64_UART_TRACE=1`
logs console input as the guest receives it.

## Tests

`target/release/rvtest <elf>...` runs riscv-tests ISA tests
(github.com/riscv-software-src/riscv-tests, `isa/`): each test starts in M mode
and reports through `tohost`. All of rv64ui, um, ua, uc, uf, ud, mi and si
pass, in both the -p (physical) and -v (virtual memory) environments, and the
same for rv32 (ELF32 tests run as RV32).

`tools/rv64-ref/lockstep.sh fw_dynamic.bin Image [max-insns]` runs the core
in lockstep with `qemu-system-riscv64` (`XLEN=32`: `qemu-system-riscv32`;
Ubuntu: `qemu-system-riscv`), comparing
pc, the general registers and the trap CSRs before every instruction. QEMU's
CPU is configured with the same extensions; where the two legitimately differ
(debug triggers, RV32 user mode, device and timer reads), lockstep adopts
QEMU's values and carries on.

## Browser

```sh
make build/v86-rv64.wasm build/libv86.js       # from the repository root
RV64_BIOS=fw_dynamic.bin RV64_KERNEL=Image RV64_ROOTFS=rootfs.squashfs tests/rv64/boot.mjs
```

```js
new V86({
    arch: "riscv64",                         // or "riscv32"
    wasm_path: "v86-rv64.wasm",
    bios: { url: "fw_dynamic.bin" },          // OpenSBI
    kernel: { url: "Image" },
    hda: { url: "rootfs.squashfs" },
    memory_size: 256 * 1024 * 1024,
    cmdline: "rootwait root=/dev/vda ro console=ttyS0",
});
```

The device tree is generated from the configured devices. The JIT is on by
default (`rv64_jit: false` turns it off; the site takes `?jit=0`).
`node tools/rv64-ref/jit-test.mjs [iterations] [seed]` runs its self-test:
random code run compiled and interpreted must end in the same state. `tools/rv64-site/`
hosts it on Cloudflare Workers, like the arm site.
