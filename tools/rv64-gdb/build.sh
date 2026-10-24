#!/bin/sh
# Builds the riscv64 root filesystem with gdb and gdb-wui
# (https://github.com/retrocpugeek/gdb-wui) for the v86-riscv site's
# "Buildroot + gdb-wui" image, whose page shows gdb-wui next to the console.
#
#   GHIDRA_INSTALL_DIR=~/ghidra tools/rv64-gdb/build.sh BUILDROOT_TREE GDB_WUI_SRC
#
# BUILDROOT_TREE: a copy of a buildroot-selfservice work dir for a riscv64 job
#   (src/ and out/, built with the gdbserver feature, so the toolchain has C++),
#   e.g. job 261017-07927b, the site's default image. The copy has its paths
#   repointed to where it is and is then built incrementally: the target gdb
#   (BR2_PACKAGE_GDB_DEBUGGER) and the overlay from tools/gdb-wui-guest
#   (gdb-wui, its example programs and S95gdb-wui, which starts it at boot).
# GDB_WUI_SRC: a gdb-wui checkout (needs Go to cross-build it).
#
# The microcontroller firmware the site's pages debug with it (?debug) goes
# in /root/mcu/<name>/: each one's ELF and sources, where their debug info
# names them. Built first: tools/ch32v003/build.sh (Tetris) and
# tools/ch32v003-gameconsole/build.sh (the game console's games).
#
# Output: build/rv64-gdb/rootfs.squashfs, for
#   tools/rv64-site/add-image.sh with WEB=/gdb-wui/ (see that script).
set -eu
TREE=$(realpath "$1")
SRC=$(realpath "$2")
cd "$(dirname "$0")/../.."
OUT=$PWD/build/rv64-gdb
DL=${BR2_DL_DIR:-$HOME/.local/share/brss/dl}
CCACHE=${CCACHE_DIR:-$HOME/.local/share/brss/ccache}
IMAGE=registry.gitlab.com/buildroot.org/buildroot/base:20250218.2110
CROSS=$TREE/out/host/bin/riscv64-buildroot-linux-gnu-
mkdir -p "$OUT"

# The tree's Makefile and .config name the directory it was built in
# (remembered in .built-in, as the first run rewrites them)
[ -f "$TREE/.built-in" ] ||
    sed -n 's|^MAKEARGS := -C \(.*\)/src$|\1|p' "$TREE/out/Makefile" > "$TREE/.built-in"
OLD=$(cat "$TREE/.built-in")
if [ "$OLD" != "$TREE" ]; then
    sed -i "s|$OLD|$TREE|g" "$TREE/out/Makefile" "$TREE/out/.config"
fi
# and so do host tools (autoconf, libtool, ...) and staging's .la and .pc files
# (all text files; package build dirs aren't rebuilt and are left as they are)
grep -rlI --null "$OLD" "$TREE/out/host" "$TREE/out/staging" 2>/dev/null |
    xargs -0 -r sed -i "s|$OLD|$TREE|g"

OVERLAY=$OUT/overlay
# (with busybox, the image's own /bin/busybox, and with GHIDRA_INSTALL_DIR the programs'
# decompilations: see overlay.sh)
"$PWD/tools/gdb-wui-guest/overlay.sh" riscv64 "$CROSS" "$SRC" "$OVERLAY" \
    "$(ls -d "$TREE"/out/build/busybox-*)/busybox"

# The firmware for the microcontroller pages' debugger (S95gdb-wui, gdb_wui.mcu)
MCU=$OVERLAY/root/mcu
mkdir -p "$MCU/tetris"
cp build/ch32v003/tetris.elf tools/ch32v003/tetris.c tools/ch32v003/start.S tools/ch32v003/libgcc.c tools/ch32v003/link.ld "$MCU/tetris/"
for dir in build/ch32v003-gameconsole/src/software/*/; do
    game=$(basename "$dir")
    # (the games the script builds; not the calibrator)
    [ -f "$dir/bin/$game.elf" ] || continue
    mkdir -p "$MCU/$game"
    cp -r "$dir/src" "$dir/bin/$game.elf" "$dir/mini-libc.c" "$MCU/$game/"
    # their licence (GPL-3.0; Conway's Life's is in its source)
    if [ -f "$dir/COPYING.txt" ]; then cp "$dir/COPYING.txt" "$MCU/$game/"; fi
done
[ -f "$MCU/tiny_tris/tiny_tris.elf" ] || { echo "no game console ELFs: run tools/ch32v003-gameconsole/build.sh" >&2; exit 1; }
if grep -rlI /home/ "$MCU" || grep -rl --binary-files=binary "$HOME" "$MCU"; then
    echo "the firmware names this machine's paths (above)" >&2
    exit 1
fi

CONFIG=$TREE/out/.config
cp "$PWD/tools/gdb-wui-guest/post-build.sh" "$OUT/post-build.sh"
POST_BUILD="$TREE/brss-post-build.sh $OUT/post-build.sh"
# expat: gdb's XML support, for the target descriptions stubs send (the
# microcontrollers' says which registers RV32E has)
sed -i -e '/BR2_PACKAGE_GDB_DEBUGGER[ =]/d' -e '/BR2_PACKAGE_EXPAT[ =]/d' -e '/^BR2_ROOTFS_OVERLAY=/d' -e '/^BR2_ROOTFS_POST_BUILD_SCRIPT=/d' "$CONFIG"
printf 'BR2_PACKAGE_GDB_DEBUGGER=y\nBR2_PACKAGE_EXPAT=y\nBR2_ROOTFS_OVERLAY="%s"\nBR2_ROOTFS_POST_BUILD_SCRIPT="%s"\n' "$OVERLAY" "$POST_BUILD" >> "$CONFIG"

# gdb was built for gdbserver alone, and a config change doesn't rebuild it:
# gdb-dirclean while the target has no gdb, or one without XML support
podman run --rm --userns=keep-id \
    -v "$TREE:$TREE:z" -v "$OUT:$OUT:z" -v "$DL:$DL:z" -v "$CCACHE:$CCACHE:z" \
    -e HOME=/tmp -e BR2_DL_DIR="$DL" -e BUILD_DIR_PREFIX="$HOME/" -e KBUILD_BUILD_USER=buildroot -e KBUILD_BUILD_HOST=buildroot \
    -w "$TREE/out" "$IMAGE" sh -c 'make olddefconfig >/dev/null &&
        { { [ -e target/usr/bin/gdb ] && ! grep -q "XML support was disabled" target/usr/bin/gdb; } || make gdb-dirclean; } && make'

cp "$TREE/out/images/rootfs.squashfs" "$OUT/rootfs.squashfs"
ls -l "$OUT/rootfs.squashfs"
