# v86-a64: AArch64 core (work in progress)

An ARMv8.0-A (AArch64 only, EL0/EL1) system emulator core, modelled on QEMU's
`virt` machine with `-cpu cortex-a53`. The goal is to boot a stock buildroot
`qemu_aarch64_virt_defconfig` kernel and rootfs in v86.

It's a standalone crate; the x86 build is unaffected. It builds both as a
native runner (`a64run`, for development and lockstep testing) and as
`build/v86-a64.wasm`, which v86 uses for `new V86({ arch: "aarch64", ... })`.

## In v86

```sh
make build/v86-a64.wasm build/libv86.js
```

```js
new V86({
    arch: "aarch64",
    wasm_path: "build/v86-a64.wasm",
    memory_size: 128 * 1024 * 1024,
    kernel: { url: "Image" },                       // arm64 Image
    hda: { url: "rootfs.ext4", async: false },       // virtio-blk, /dev/vda
    cmdline: "rootwait root=/dev/vda console=ttyAMA0",
    net_device: { relay_url: "fetch", type: "virtio" }, // optional
    serial_container: document.getElementById("serial"),
    autostart: true,
});
```

The wasm core holds the CPU, MMU, GIC and timers. Device space is forwarded to
JS (`src/a64_cpu.js`): PL011 on `serial0` (`src/pl011.js`), PL031
(`src/pl031.js`), and virtio-mmio slots. v86's existing virtio devices
(virtio-net, virtio-console, 9p) run on the virtio-mmio transport added to
`src/virtio.js`, and `src/virtio_blk.js` serves `hda`. The device tree is
generated from the configured devices (`src/fdt.js`); `dtb` overrides it.
Guest time follows the host clock, and idle time is waited out rather than skipped.
State save/restore isn't supported yet.

Tests (they need the buildroot images):

```sh
A64_KERNEL=.../Image A64_ROOTFS=.../rootfs.ext4 tests/a64/boot.mjs   # node, V86 API
A64_NET=fetch A64_KERNEL=... A64_ROOTFS=... tests/a64/boot.mjs       # plus virtio-net/DHCP
tests/a64/browser.mjs     # examples/aarch64.html in headless Chrome (images in images/aarch64/)
```

## Layout

| File | Contents |
|---|---|
| `cpu.rs` | State, step/run loop, exception entry/ERET, generic timers |
| `mmu.rs` | VMSAv8-64 stage 1 walk (4 KB granule, TBI), ASID-tagged TLB, memory accessors |
| `decode.rs` | Decoded-instruction cache: per-page decoded ops with handlers for the common integer forms (others go through `exec`) |
| `jit.rs` | Browser build: compiles hot code to WebAssembly a page at a time (see below) |
| `sysregs.rs` | MRS/MSR, EL0 trap rules; ID values captured from QEMU (`tools/a64-ref/idprobe`) |
| `exec_int.rs` | Integer data processing, branches |
| `exec_ldst.rs` | Loads/stores, pairs, exclusives, acquire/release |
| `exec_simd.rs` | AdvSIMD integer subset, scalar FP, structure loads/stores, PMULL |
| `exec_crypto.rs` | AES, SHA1, SHA256 instructions |
| `exec_sys.rs` | System instructions, cache/TLB maintenance, PSCI (HVC conduit) |
| `gic.rs` | GICv2 distributor + CPU interface (uniprocessor) |
| `dev/` | PL011, PL031, virtio-mmio (v2) + virtio-blk; native runner only |
| `boot.rs` | QEMU-identical `-kernel` layout (boot stub, Image at RAM+2 MB, DTB at RAM+64 MB) |

## Running

```sh
cargo build --release            # from src/a64
dtc -I dts -O dtb -o virt.dtb ../../tools/a64-ref/v86-virt.dts
target/release/a64run --kernel Image --dtb virt.dtb --disk rootfs.ext4
```

The serial console is on stdin/stdout; Ctrl-A x quits. The disk is copy-on-write
in memory. `tools/a64-ref/smoke.py` boots, logs in as root, runs a few commands
and powers off (`--qemu` runs the same thing on QEMU).

## JIT (browser build)

`jit.rs` compiles hot code to WebAssembly a page at a time. Once a branch
target has been entered 100 times, its page is compiled into one function
that holds all the page's hot entry points and the code reachable from them
within the page. It dispatches on pc in a loop, so loops, calls and returns
within the page stay in compiled code. A new hot entry point recompiles the
page. JS compiles the module and adds it to the core's function table
(`A64CPU.jit_compile`).

Compiled code works on the `Cpu` struct in linear memory and looks loads and
stores up in the TLB inline. Accesses to anything but RAM go through
`jit_read`/`jit_write` and return to the run loop after the instruction, so
device interrupts are seen at the same point as when interpreting. The first
time a call runs a block, it checks that the block's instruction words in RAM
are still the ones it was compiled from, so writes to code and DMA need no
tracking. Instructions it doesn't compile, and rare cases of those it does
(a Q-register access outside the TLB's RAM entries, for example), are left
to the interpreter. `?jit=0` on the site turns it off.

The native build has no JIT. Its self-test runs random code, mostly
branching within itself, compiled and interpreted from the same random state
(with the MMU off, and on with random TLB permissions) and compares the
results:

```sh
make build/v86-a64.wasm && node tools/a64-ref/jit-test.mjs [iterations] [seed]
```

## Lockstep against QEMU

```sh
DISK=rootfs.ext4 tools/a64-ref/lockstep.sh Image [max-instructions]
```

QEMU runs with `one-insn-per-tb`, `-icount shift=0,sleep=off` and `-d cpu,int,exec`;
its trace is streamed through a FIFO and compared against our PC, X0-X30, SP
and PSTATE before every instruction. The comparator also handles:

- IRQs are injected where QEMU took them. QEMU's timer callbacks run on the
  main-loop thread, so their position in the instruction stream isn't deterministic.
- TBs that QEMU logged but exited before executing ("Stopped execution of TB chain").
- Instruction-fetch aborts, which QEMU doesn't log.
- Values from device registers that differ from QEMU's ("mmio-follow"): QEMU's
  value is adopted and logged, so one run lists all device-model differences.

With the buildroot rootfs this matches for about 77M instructions, through
kernel boot, disk I/O and the first user processes. It stops when QEMU's
asynchronous block I/O completes requests in a different order.
