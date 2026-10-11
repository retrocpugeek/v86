#!/bin/sh
# Run the a64 core in lockstep with QEMU (cortex-a53, virt) on the same kernel.
#
#   tools/a64-ref/lockstep.sh path/to/Image [max-instructions] [extra a64run args...]
#
# QEMU's per-instruction register dump (-d cpu with one-insn-per-tb) is streamed
# through a FIFO, so nothing large is written to disk.
set -eu

IMAGE=${1:?usage: lockstep.sh Image [max-insns]}
STEPS=${2:-100000000}
shift; [ $# -gt 0 ] && shift
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
WORK=${A64_WORK:-$(mktemp -d)}

dtc -I dts -O dtb -o "$WORK/v86-virt.dtb" "$HERE/v86-virt.dts"

# icount: QEMU's virtual clock advances 2^shift ns per instruction, idle time is
# skipped (sleep=off) - the same time model a64run uses with --icount-shift.
SHIFT=${ICOUNT_SHIFT:-0}
QEMU="qemu-system-aarch64 -M virt,dtb-randomness=off -cpu cortex-a53 -smp 1 -m 128 \
  -display none -serial null -monitor none -accel tcg,one-insn-per-tb=on \
  -icount shift=$SHIFT,sleep=off -global virtio-mmio.force-legacy=false \
  -kernel $IMAGE -dtb $WORK/v86-virt.dtb"

# Optional root disk (DISK=rootfs.ext4); QEMU opens it read-only via snapshot=on.
DISK_QEMU=""
DISK_A64=""
if [ -n "${DISK:-}" ]; then
  DISK_QEMU="-drive file=$DISK,if=none,format=raw,id=hd0,snapshot=on -device virtio-blk-device,drive=hd0"
  DISK_A64="--disk $DISK"
fi
QEMU="$QEMU $DISK_QEMU"

# QEMU patches the DTB it is given (e.g. /chosen); boot ours with the final version.
$QEMU -machine dumpdtb="$WORK/final.dtb" >/dev/null 2>&1 || true

(cd "$ROOT/src/a64" && cargo build --release -q)

rm -f "$WORK/trace.fifo"
mkfifo "$WORK/trace.fifo"
$QEMU -d ${QEMU_LOG:-cpu,int,exec},nochain -D "$WORK/trace.fifo" &
QPID=$!
trap 'kill $QPID 2>/dev/null || true; rm -f "$WORK/trace.fifo"' EXIT

"$ROOT/src/a64/target/release/a64run" --kernel "$IMAGE" --dtb "$WORK/final.dtb" \
  --lockstep "$WORK/trace.fifo" --steps "$STEPS" --icount-shift "$SHIFT" $DISK_A64 "$@"
