#!/bin/sh
# Builds U-Boot for v86's aarch64 machine and a disk for it to boot: an ext4
# boot partition with the kernel and extlinux/extlinux.conf, then a squashfs
# root filesystem.
#
#   tools/a64-site/u-boot/build.sh <Image> <rootfs.squashfs> <outdir>
#
# Writes <outdir>/u-boot.bin and <outdir>/disk.img, then add them to the site:
#   FIRMWARE=1 tools/a64-site/add-image.sh u-boot "U-Boot -> Buildroot" \
#       <outdir>/u-boot.bin <outdir>/disk.img
#
# Needs an aarch64 cross compiler (CROSS_COMPILE, default aarch64-linux-gnu-),
# bison and flex (a Buildroot output's host/bin has them), mkfs.ext4 and sfdisk.
set -eu
[ $# -eq 3 ] || { echo "usage: $0 <Image> <rootfs.squashfs> <outdir>" >&2; exit 1; }
KERNEL=$(realpath "$1") ROOTFS=$(realpath "$2") OUT=$(realpath -m "$3")
HERE=$(cd "$(dirname "$0")" && pwd)
VERSION=v2026.10
CROSS_COMPILE=${CROSS_COMPILE:-aarch64-linux-gnu-}
# The version banner's build time, in UTC rather than the build machine's zone
export TZ=UTC
export PATH=$PATH:/sbin:/usr/sbin
mkdir -p "$OUT"

# U-Boot
[ -d "$OUT/src" ] || git -c advice.detachedHead=false clone -q --depth 1 --branch $VERSION https://github.com/u-boot/u-boot.git "$OUT/src"
cp "$HERE/v86.env" "$OUT/src/board/emulation/qemu-arm/v86.env"
B=$OUT/build
make -s -C "$OUT/src" CROSS_COMPILE="$CROSS_COMPILE" O="$B" qemu_arm64_defconfig
"$OUT/src/scripts/kconfig/merge_config.sh" -m -O "$B" "$B/.config" "$HERE/v86.config" >/dev/null
make -s -C "$OUT/src" CROSS_COMPILE="$CROSS_COMPILE" O="$B" olddefconfig
make -s -C "$OUT/src" CROSS_COMPILE="$CROSS_COMPILE" O="$B" -j"$(nproc)"
cp "$B/u-boot.bin" "$OUT/u-boot.bin"

# Boot partition: 1 MB in, sized for the kernel; the root filesystem follows
MB=1048576
rm -rf "$OUT/boot" && mkdir -p "$OUT/boot/extlinux"
cp "$KERNEL" "$OUT/boot/Image"
cat > "$OUT/boot/extlinux/extlinux.conf" <<'CONF'
# Read by U-Boot's standard boot (bootflow scan): the kernel on this
# partition, with the squashfs root filesystem on partition 2.
menu title v86 aarch64: U-Boot -> Buildroot
timeout 30
default buildroot

label buildroot
  menu label Buildroot (squashfs root on /dev/vda2)
  kernel /Image
  append rootwait root=/dev/vda2 rootfstype=squashfs console=ttyAMA0
CONF
BOOT_MB=$(( $(stat -Lc %s "$KERNEL") / MB + 8 ))
ROOT_MB=$(( ( $(stat -Lc %s "$ROOTFS") + MB - 1 ) / MB ))
rm -f "$OUT/boot.ext4" "$OUT/disk.img"
mkfs.ext4 -q -F -L boot -O ^has_journal -d "$OUT/boot" "$OUT/boot.ext4" "${BOOT_MB}M"
truncate -s $(( (1 + BOOT_MB + ROOT_MB) * MB )) "$OUT/disk.img"
sfdisk -q "$OUT/disk.img" <<PARTS
label: dos
start=2048, size=$(( BOOT_MB * 2048 )), type=83, bootable
start=$(( (1 + BOOT_MB) * 2048 )), type=83
PARTS
dd if="$OUT/boot.ext4" of="$OUT/disk.img" bs=$MB seek=1 conv=notrunc status=none
dd if="$ROOTFS" of="$OUT/disk.img" bs=$MB seek=$(( 1 + BOOT_MB )) conv=notrunc status=none
echo "$OUT/u-boot.bin, $OUT/disk.img ($(( (1 + BOOT_MB + ROOT_MB) )) MB)"
