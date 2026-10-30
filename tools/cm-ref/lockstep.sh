#!/bin/sh
# Compares the Cortex-M core (src/cm, cmrun) with Pebble's QEMU instruction by
# instruction: QEMU's register dumps (-d exec,cpu with one instruction per
# block and -icount shift=4, 16 ns an instruction) against cmrun's (CM_REGS,
# CM_NS16), and prints the first instruction where they differ.
#
#   PEBBLE=~/repo/pebble tools/cm-ref/lockstep.sh [board] [instructions]
#
# PEBBLE: a directory with a PebbleOS checkout built for the board
# (pebbleos/build-<board>: pebbleos.elf, qemu_spi_flash.bin), the PebbleOS
# SDK (home/pebbleos-sdk-*) and the pebbleos-build container image, as set up
# in the Pebble emulation notes. QEMU runs in that container.
#
# Values from the host's clock differ (the RTC): the first RTC read is the
# first difference unless CM_EPOCH is set to the time QEMU read (seconds).
set -eu
BOARD=${1:-flint}
N=${2:-2000000}
PEBBLE=${PEBBLE:-$HOME/repo/pebble}
HERE=$(cd "$(dirname "$0")" && pwd)
CM=$HERE/../../src/cm
OUT=${OUT:-$(mktemp -d)}
B=$PEBBLE/pebbleos/build-$BOARD
AUDIO=
[ "$BOARD" = gabbro ] || AUDIO=",audiodev=snd0 -audiodev none,id=snd0"
podman run --rm --userns=keep-id -v "$PEBBLE:/work:z" -v "$OUT:/out:z" -w "/work/pebbleos/build-$BOARD" -e HOME=/work/home pebbleos-build bash -c "
. /work/home/pebbleos-sdk-*/env.sh
timeout 25 qemu-pebble -serial null -serial null -serial null -machine pebble-$BOARD$AUDIO \
  -kernel pebbleos.elf -drive if=mtd,format=raw,file=qemu_spi_flash.bin -display none \
  -icount shift=4 -accel tcg,one-insn-per-tb=on -d exec,cpu,nochain -D /out/qemu-cpu.log" || true
python3 "$HERE/qemu-regs.py" "$OUT/qemu-cpu.log" "$N" > "$OUT/qemu-regs.txt"
rm -f "$OUT/qemu-cpu.log"
(cd "$CM" && cargo build -q --release)

env CM_NS16=1 CM_REGS=1 CM_PCTRACE=$N ${CM_EPOCH:+CM_EPOCH=$CM_EPOCH} "$CM/target/release/cmrun" --board "$BOARD" --elf "$B/pebbleos.elf" --spi "$B/qemu_spi_flash.bin" > "$OUT/cm-regs.txt"
python3 "$HERE/cmp-regs.py" "$OUT/cm-regs.txt" "$OUT/qemu-regs.txt"
echo "(traces in $OUT)"
