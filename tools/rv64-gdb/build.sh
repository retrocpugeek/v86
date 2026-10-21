#!/bin/sh
# Builds the riscv64 root filesystem with gdb and gdb-wui
# (https://github.com/retrocpugeek/gdb-wui) for the v86-riscv site's
# "Buildroot + gdb-wui" image, whose page shows gdb-wui next to the console.
#
#   tools/rv64-gdb/build.sh BUILDROOT_TREE GDB_WUI_SRC
#
# BUILDROOT_TREE: a copy of a buildroot-selfservice work dir for a riscv64 job
#   (src/ and out/, built with the gdbserver feature, so the toolchain has C++),
#   e.g. job 261017-07927b, the site's default image. The copy has its paths
#   repointed to where it is and is then built incrementally: the target gdb
#   (BR2_PACKAGE_GDB_DEBUGGER) and an overlay with gdb-wui, its demo programs
#   (gdb-wui's testdata/fixtures, compiled as its Makefile does) and
#   S95gdb-wui, which starts gdb-wui at boot.
# GDB_WUI_SRC: a gdb-wui checkout (needs Go to cross-build it).
#
# Output: build/rv64-gdb/rootfs.squashfs, for
#   tools/rv64-site/add-image.sh with WEB=/gdb-wui/ (see that script).
set -eu
TREE=$(realpath "$1")
SRC=$(realpath "$2")
cd "$(dirname "$0")/../.."
HERE=$PWD/tools/rv64-gdb
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
rm -rf "$OVERLAY"
mkdir -p "$OVERLAY/usr/bin" "$OVERLAY/root/demo" "$OVERLAY/etc/init.d"

(cd "$SRC" && GOOS=linux GOARCH=riscv64 CGO_ENABLED=0 \
    go build -trimpath -ldflags="-s -w" -o "$OVERLAY/usr/bin/gdb-wui" ./cmd/gdb-wui)

for f in "$SRC"/testdata/fixtures/*.c; do
    n=$(basename "$f" .c)
    cp "$f" "$OVERLAY/root/demo/"
    case $n in
        # as in gdb-wui's Makefile: one stripped, one with symbols but no debug info
        nodebug) "${CROSS}gcc" -O0 -o "$OVERLAY/root/demo/$n" "$f" && "${CROSS}strip" "$OVERLAY/root/demo/$n" ;;
        minsym) "${CROSS}gcc" -O0 -o "$OVERLAY/root/demo/$n" "$f" ;;
        # (the debug info names the source as /root/demo, and the toolchain's headers as under /usr)
        *) "${CROSS}gcc" -g -O0 -pthread -fdebug-prefix-map="$SRC/testdata/fixtures=/root/demo" -fdebug-prefix-map="$PWD=/root/demo" \
               -fdebug-prefix-map="$TREE/out/host/riscv64-buildroot-linux-gnu/sysroot=/" \
               -fdebug-prefix-map="$TREE/out/host=/usr" \
               -o "$OVERLAY/root/demo/$n" "$f" ;;
    esac
done
cp "$HERE/S95gdb-wui" "$OVERLAY/etc/init.d/"
cat > "$OVERLAY/etc/motd" <<'EOF'
gdb-wui is running on this machine: its window is next to this console.
Its project is /root/demo (gdb-wui's example programs, with their sources).
EOF

CONFIG=$TREE/out/.config
cp "$HERE/post-build.sh" "$OUT/post-build.sh"
POST_BUILD="$TREE/brss-post-build.sh $OUT/post-build.sh"
sed -i -e '/BR2_PACKAGE_GDB_DEBUGGER[ =]/d' -e '/^BR2_ROOTFS_OVERLAY=/d' -e '/^BR2_ROOTFS_POST_BUILD_SCRIPT=/d' "$CONFIG"
printf 'BR2_PACKAGE_GDB_DEBUGGER=y\nBR2_ROOTFS_OVERLAY="%s"\nBR2_ROOTFS_POST_BUILD_SCRIPT="%s"\n' "$OVERLAY" "$POST_BUILD" >> "$CONFIG"

# gdb was built for gdbserver alone, and a config change doesn't rebuild it:
# gdb-dirclean while the target has no gdb
podman run --rm --userns=keep-id \
    -v "$TREE:$TREE:z" -v "$OUT:$OUT:z" -v "$DL:$DL:z" -v "$CCACHE:$CCACHE:z" \
    -e HOME=/tmp -e BR2_DL_DIR="$DL" -e BUILD_DIR_PREFIX="$HOME/" -e KBUILD_BUILD_USER=buildroot -e KBUILD_BUILD_HOST=buildroot \
    -w "$TREE/out" "$IMAGE" sh -c 'make olddefconfig >/dev/null &&
        { [ -e target/usr/bin/gdb ] || make gdb-dirclean; } && make'

cp "$TREE/out/images/rootfs.squashfs" "$OUT/rootfs.squashfs"
ls -l "$OUT/rootfs.squashfs"
