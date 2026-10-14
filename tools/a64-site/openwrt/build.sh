#!/bin/sh
# Builds an OpenWrt armsr/armv8 image for v86's aarch64 machine with the
# official image builder (no compiling): the stock kernel, and a squashfs
# rootfs without the drivers for real hardware, with LAN set up for v86's
# DHCP/DNS (files/etc/uci-defaults/99-v86).
#
#   tools/a64-site/openwrt/build.sh <work-dir>
#   tools/a64-site/add-image.sh openwrt "OpenWrt 24.10.8" \
#       <work-dir>/out/openwrt-24.10.8-armsr-armv8-generic-kernel.bin \
#       <work-dir>/out/openwrt-24.10.8-armsr-armv8-generic-squashfs-rootfs.img \
#       "rootwait root=/dev/vda console=ttyAMA0"
set -eu
VERSION=${OPENWRT_VERSION:-24.10.8}
WORK=$(realpath "${1:?usage: $0 <work-dir>}")
HERE=$(cd "$(dirname "$0")" && pwd)
URL=https://downloads.openwrt.org/releases/$VERSION/targets/armsr/armv8
IB=openwrt-imagebuilder-$VERSION-armsr-armv8.Linux-x86_64

mkdir -p "$WORK"
cd "$WORK"
if [ ! -d "$IB" ]; then
    curl -fO "$URL/$IB.tar.zst"
    curl -fs "$URL/sha256sums" | grep " \*\?$IB.tar.zst\$" | sha256sum -c
    tar -I zstd -xf "$IB.tar.zst"
fi

# The generic profile's hardware drivers, and PPPoE
REMOVE="kmod-amazon-ena kmod-e1000e kmod-vmxnet3 kmod-rtc-rx8025 kmod-i2c-mux-pca954x
    kmod-gpio-pca953x kmod-wdt-sp805 kmod-mvneta kmod-mvpp2 kmod-fsl-dpaa1-net
    kmod-fsl-dpaa2-net kmod-fsl-enetc-net kmod-dwmac-imx kmod-fsl-fec kmod-dwmac-rockchip
    kmod-dwmac-sun8i kmod-phy-aquantia kmod-phy-broadcom kmod-phy-marvell
    kmod-phy-marvell-10g kmod-sfp kmod-atlantic kmod-bcmgenet kmod-octeontx2-net
    kmod-renesas-net-avb kmod-phy-realtek kmod-phy-smsc kmod-thunderx-net
    ppp ppp-mod-pppoe"
PACKAGES=$(for p in $REMOVE; do printf -- "-%s " "$p"; done)

make -C "$IB" image PROFILE=generic PACKAGES="$PACKAGES" FILES="$HERE/files" \
    ROOTFS_PARTSIZE=64 BIN_DIR="$WORK/out"
cd "$WORK/out"
gunzip -kf "openwrt-$VERSION-armsr-armv8-generic-squashfs-rootfs.img.gz"
ls -la "$WORK/out"
