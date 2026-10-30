#!/bin/sh
# Builds PebbleOS (https://github.com/coredevices/PebbleOS, Apache-2.0) for one
# of its emulator boards, for the v86-arm site's Pebble watch (v86's
# Cortex-M core, src/cm): the firmware ELF, stripped, and the external
# flash's image (resources and file system), gzipped.
#
#   PEBBLE=~/repo/pebble tools/a64-site/pebble/build.sh [flint|emery|gabbro] [out-dir]
#
# PEBBLE: a directory with a PebbleOS checkout (pebbleos/, with its .venv
# set up), the PebbleOS SDK installed in home/ (its installer with
# --prefix /work/home/pebbleos-sdk-<version>) and the container image
# pebbleos-build (container/Containerfile: Ubuntu 24.04 with the packages
# PebbleOS's getting-started guide lists). The build runs in that container,
# with only $PEBBLE mounted (as /work).
#
# Writes firmware.elf and spi.bin.gz to out-dir (default
# build/pebble/<board>), then:
#   tools/a64-site/add-watch.sh pebble-flint "Pebble 2 Duo: PebbleOS" flint build/pebble/flint
set -eu
BOARD=${1:-flint}
OUT=$(realpath -m "${2:-build/pebble/$BOARD}")
PEBBLE=$(realpath "${PEBBLE:-$HOME/repo/pebble}")
case "$BOARD" in flint|emery|gabbro) ;; *) echo "board: flint, emery or gabbro" >&2; exit 1;; esac
podman run --rm --userns=keep-id -v "$PEBBLE:/work:z" -w /work/pebbleos -e HOME=/work/home pebbleos-build bash -c "
set -e
. /work/home/pebbleos-sdk-*/env.sh
. .venv/bin/activate
pbl -b build-$BOARD configure --board qemu_$BOARD >/dev/null
pbl -b build-$BOARD build | tail -3
cmake --build build-$BOARD --target=qemu_image_micro --target=qemu_image_spi >/dev/null
arm-none-eabi-strip -o build-$BOARD/firmware.elf build-$BOARD/pebbleos.elf
git describe --tags --always > build-$BOARD/version.txt"
B=$PEBBLE/pebbleos/build-$BOARD
mkdir -p "$OUT"
cp "$B/firmware.elf" "$B/version.txt" "$OUT/"
gzip -9 -c "$B/qemu_spi_flash.bin" > "$OUT/spi.bin.gz"
# (built in the container, under /work: no paths of this machine)
if grep -l --binary-files=binary "$HOME" "$OUT/firmware.elf" "$B/qemu_spi_flash.bin"; then
    echo "the firmware names this machine's paths (above)" >&2
    exit 1
fi
ls -l "$OUT"
