#!/bin/sh
# Builds the gdb-wui example image (Buildroot): kernel and squashfs root
# filesystem with gdb, and gdb-wui with its example programs
# (tools/gdb-wui-guest/overlay.sh, from a gdb-wui checkout; needs Go).
#
#   GHIDRA_INSTALL_DIR=~/ghidra tools/a64-site/gdb-wui/build.sh <buildroot source> <output dir> <gdb-wui source>
#   WEB=/gdb-wui/ WEB_APP=gdb-wui MEMORY=256 tools/a64-site/add-image.sh gdb-wui "..." \
#       <output>/images/Image <output>/images/rootfs.squashfs \
#       "rootwait root=/dev/vda ro rootfstype=squashfs console=ttyAMA0"
set -eu
[ $# -eq 3 ] || { echo "usage: $0 <buildroot source> <output dir> <gdb-wui source>" >&2; exit 1; }
HERE=$(cd "$(dirname "$0")" && pwd)
# Keep the build machine's user and host name out of the kernel's version
# string (/proc/version), as Buildroot does for BR2_REPRODUCIBLE
export KBUILD_BUILD_USER=buildroot KBUILD_BUILD_HOST=buildroot
BR=$(realpath "$1") OUT=$(realpath -m "$2") SRC=$(realpath "$3")
mkdir -p "$OUT"
sed -e "s|@HERE@|$HERE|g" -e "s|@OVERLAY@|$OUT/gdb-wui-overlay|g" "$HERE/gdb-wui_defconfig" > "$OUT/gdb-wui_defconfig"
make -C "$BR" O="$OUT" BR2_DEFCONFIG="$OUT/gdb-wui_defconfig" defconfig
# The overlay's programs need the toolchain, and it takes busybox's build into
# the demo: build those first. With GHIDRA_INSTALL_DIR set it also
# decompiles them (see overlay.sh).
make -C "$OUT" toolchain busybox
"$HERE/../../gdb-wui-guest/overlay.sh" arm64 "$OUT/host/bin/aarch64-buildroot-linux-gnu-" "$SRC" "$OUT/gdb-wui-overlay" \
    "$(ls -d "$OUT"/build/busybox-*)/busybox"
make -C "$OUT"
