# v86-a64: AArch64 core (work in progress)

An ARMv8.0-A (AArch64 only, EL0/EL1) system emulator core, modelled on QEMU's
`virt` machine with `-cpu cortex-a53`. The goal is to boot a stock buildroot
`qemu_aarch64_virt_defconfig` kernel and rootfs in v86.

It's a standalone crate for now; the x86 build is unaffected. Only the native
runner exists so far (no wasm build or browser integration yet).

## Layout

| File | Contents |
|---|---|
| `cpu.rs` | State, step/run loop, exception entry/ERET, generic timers |
| `mmu.rs` | VMSAv8-64 stage 1 walk (4 KB granule, TBI), ASID-tagged TLB, memory accessors |
| `sysregs.rs` | MRS/MSR, EL0 trap rules; ID values captured from QEMU (`tools/a64-ref/idprobe`) |
| `exec_int.rs` | Integer data processing, branches |
| `exec_ldst.rs` | Loads/stores, pairs, exclusives, acquire/release |
| `exec_simd.rs` | AdvSIMD integer subset, scalar FP, structure loads/stores, PMULL |
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
