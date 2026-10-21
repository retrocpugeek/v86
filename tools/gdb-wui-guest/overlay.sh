#!/bin/sh
# Builds the root filesystem overlay for a "Buildroot + gdb-wui" image: gdb-wui
# (https://github.com/retrocpugeek/gdb-wui) cross-built with Go, its example
# programs (testdata/fixtures, compiled as its Makefile does, with their
# sources) in /root/demo, and S95gdb-wui, which starts it at boot. The image
# also needs gdb (BR2_PACKAGE_GDB_DEBUGGER) and socat.
#
#   tools/gdb-wui-guest/overlay.sh <GOARCH> <cross prefix> <gdb-wui source> <overlay dir>
#
# e.g. overlay.sh arm64 .../host/bin/aarch64-buildroot-linux-gnu- ~/gdb-wui out/overlay
set -eu
[ $# -eq 4 ] || { echo "usage: $0 <GOARCH> <cross prefix> <gdb-wui source> <overlay dir>" >&2; exit 1; }
GOARCH=$1 CROSS=$2 SRC=$(realpath "$3") OVERLAY=$(realpath -m "$4")
HERE=$(cd "$(dirname "$0")" && pwd)
# The toolchain's own directories, which the debug info would otherwise name
HOST=$(realpath "$(dirname "$CROSS")/..")
SYSROOT=$("${CROSS}gcc" -print-sysroot)

rm -rf "${OVERLAY:?}"
mkdir -p "$OVERLAY/usr/bin" "$OVERLAY/root/demo" "$OVERLAY/etc/init.d"

(cd "$SRC" && GOOS=linux GOARCH=$GOARCH CGO_ENABLED=0 \
    go build -trimpath -ldflags="-s -w" -o "$OVERLAY/usr/bin/gdb-wui" ./cmd/gdb-wui)

cd "$SRC/testdata/fixtures"
for f in *.c; do
    n=${f%.c}
    cp "$f" "$OVERLAY/root/demo/"
    case $n in
        # as in gdb-wui's Makefile: one stripped, one with symbols but no debug info
        nodebug) "${CROSS}gcc" -O0 -o "$OVERLAY/root/demo/$n" "$f" && "${CROSS}strip" "$OVERLAY/root/demo/$n" ;;
        minsym) "${CROSS}gcc" -O0 -o "$OVERLAY/root/demo/$n" "$f" ;;
        # the debug info names the sources as /root/demo, and the toolchain's headers as under /usr
        *) "${CROSS}gcc" -g -O0 -pthread -fdebug-prefix-map="$PWD=/root/demo" \
               -fdebug-prefix-map="$SYSROOT=/" -fdebug-prefix-map="$HOST=/usr" \
               -o "$OVERLAY/root/demo/$n" "$f" ;;
    esac
done

cp "$HERE/S95gdb-wui" "$OVERLAY/etc/init.d/"
cat > "$OVERLAY/etc/motd" <<'EOF'
gdb-wui is running on this machine: its window is next to this console.
Its project is /root/demo (gdb-wui's example programs, with their sources).
To debug under gdbserver: gdbserver :1234 /root/demo/hello &
and click "connect" in gdb-wui's console bar.
EOF
