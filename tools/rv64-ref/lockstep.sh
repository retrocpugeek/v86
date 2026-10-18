#!/bin/sh
# Run the rv64 core in lockstep with QEMU (virt, RV64IMAFDC, or with XLEN=32
# RV32IMAFDC) on the same firmware and kernel.
#
#   tools/rv64-ref/lockstep.sh fw_dynamic.bin Image [max-instructions] [extra rv64run args...]
#
# DISK=rootfs.squashfs attaches a root disk (QEMU puts it on the last
# virtio-mmio transport, so the device tree gets a node for it there).
# QEMU's per-instruction register dump (-d cpu with one-insn-per-tb) is
# streamed through a FIFO, so nothing large is written to disk.
set -eu

FW=${1:?usage: lockstep.sh fw_dynamic.bin Image [max-insns]}
IMAGE=${2:?usage: lockstep.sh fw_dynamic.bin Image [max-insns]}
STEPS=${3:-100000000}
shift 2; [ $# -gt 0 ] && shift
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
WORK=${RV64_WORK:-$(mktemp -d)}
XLEN=${XLEN:-64}

if [ "$XLEN" = 32 ]; then
  DTS=$HERE/virt32.dts SV=sv32=on XLEN_ARG="--xlen 32"
else
  DTS=$HERE/virt.dts SV=sv39=on XLEN_ARG=""
fi
if [ -n "${DISK:-}" ]; then
  sed 's|^\(\t*\)virtio_mmio@10002000 {|\1virtio_mmio@10008000 {\n\1\tcompatible = "virtio,mmio";\n\1\treg = <0x0 0x10008000 0x0 0x1000>;\n\1\tinterrupts = <8>;\n\1\tinterrupt-parent = <\&plic>;\n\1};\n\n\1virtio_mmio@10002000 {|' \
    "$DTS" > "$WORK/virt.dts"
  DTS=$WORK/virt.dts
fi
dtc -I dts -O dtb -o "$WORK/virt.dtb" "$DTS"

# The bare rv64i/rv32i CPU with exactly the extensions the core implements.
CPU="rv${XLEN}i,m=on,a=on,f=on,d=on,c=on,zicsr=on,zifencei=on,zicntr=on,zihpm=on,pmu-mask=0,s=on,u=on,mmu=on,$SV,pmp=on,debug=off"
# icount: QEMU's virtual clock advances 1 ns per instruction and idle time is
# skipped (sleep=off), the time model rv64run uses.
QEMU="qemu-system-riscv$XLEN -M virt -cpu $CPU -smp 1 -m 256 \
  -display none -serial null -monitor none -accel tcg,one-insn-per-tb=on \
  -icount shift=0,sleep=off -global virtio-mmio.force-legacy=false \
  -bios $FW -kernel $IMAGE -dtb $WORK/virt.dtb"

DISK_ARG=""
if [ -n "${DISK:-}" ]; then
  QEMU="$QEMU -drive file=$DISK,if=none,format=raw,id=hd0,snapshot=on -device virtio-blk-device,drive=hd0"
  DISK_ARG="--disk $DISK --disk-slot 7"
fi

# QEMU patches the DTB it is given (/chosen); boot ours with the final version.
$QEMU -machine dumpdtb="$WORK/final.dtb" >/dev/null 2>&1 || true

(cd "$ROOT/src/rv64" && cargo build --release -q)

rm -f "$WORK/trace.fifo"
mkfifo "$WORK/trace.fifo"
$QEMU -d ${QEMU_LOG:-cpu,int,exec},nochain -D "$WORK/trace.fifo" &
QPID=$!
trap 'kill $QPID 2>/dev/null || true; rm -f "$WORK/trace.fifo"' EXIT

"$ROOT/src/rv64/target/release/rv64run" --bios "$FW" --kernel "$IMAGE" --dtb "$WORK/final.dtb" \
  --lockstep "$WORK/trace.fifo" --steps "$STEPS" $XLEN_ARG $DISK_ARG "$@"
