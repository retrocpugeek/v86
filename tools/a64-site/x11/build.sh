#!/bin/sh
# Builds the X11 example image (Buildroot): kernel and ext4 root filesystem.
#
#   tools/a64-site/x11/build.sh <buildroot source> <output dir>
#   SCREEN=800x600 MEMORY=256 tools/a64-site/add-image.sh x11 "..." \
#       <output>/images/Image <output>/images/rootfs.ext4
set -eu
[ $# -eq 2 ] || { echo "usage: $0 <buildroot source> <output dir>" >&2; exit 1; }
HERE=$(cd "$(dirname "$0")" && pwd)
# Keep the build machine's user and host name out of the kernel's version
# string (/proc/version), as Buildroot does for BR2_REPRODUCIBLE
export KBUILD_BUILD_USER=buildroot KBUILD_BUILD_HOST=buildroot
BR=$(realpath "$1") OUT=$(realpath -m "$2")
mkdir -p "$OUT"
sed "s|@HERE@|$HERE|g" "$HERE/x11_defconfig" > "$OUT/x11_defconfig"
make -C "$BR" O="$OUT" BR2_DEFCONFIG="$OUT/x11_defconfig" defconfig
# openbox's install races with itself under make -j; a second run finishes it
make -C "$OUT" || make -C "$OUT"
