#!/bin/sh
# Builds the root filesystem overlay for a "Buildroot + gdb-wui" image: gdb-wui
# (https://github.com/retrocpugeek/gdb-wui) cross-built with Go, its example
# programs (testdata/fixtures, compiled as its Makefile does, with their
# sources) in /root/demo, and S95gdb-wui, which starts it at boot. The image
# also needs gdb (BR2_PACKAGE_GDB_DEBUGGER) and socat.
#
#   tools/gdb-wui-guest/overlay.sh <GOARCH> <cross prefix> <gdb-wui source> <overlay dir> [busybox]
#
# e.g. overlay.sh arm64 .../host/bin/aarch64-buildroot-linux-gnu- ~/gdb-wui out/overlay \
#        out/build/busybox-1.37.0/busybox_unstripped
#
# [busybox], Buildroot's unstripped busybox, goes into /root/demo too: a real
# program of a few thousand functions to read decompiled.
#
# With Ghidra (GHIDRA_INSTALL_DIR), every program in /root/demo is decompiled
# here, by gdb-wui -write-sidecar, into /usr/share/gdb-wui/sidecars, which
# S95gdb-wui passes as -decomp-sidecars: the Decompiled tab then works in the
# guest, where Ghidra itself could never run.
set -eu
[ $# -eq 4 ] || [ $# -eq 5 ] || { echo "usage: $0 <GOARCH> <cross prefix> <gdb-wui source> <overlay dir> [busybox]" >&2; exit 1; }
GOARCH=$1 CROSS=$2 SRC=$(realpath "$3") OVERLAY=$(realpath -m "$4") BUSYBOX=${5:-}
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

if [ -n "$BUSYBOX" ]; then
    # Its symbols, which are the point, but not the scraps of debug info
    # libgcc brings, which name the directory the toolchain was built in
    "${CROSS}strip" --strip-debug -o "$OVERLAY/root/demo/busybox" "$BUSYBOX"
fi

if [ -n "${GHIDRA_INSTALL_DIR:-}" ]; then
    # gdb-wui for this machine, to write them with
    HOSTBIN=$(mktemp -d)
    (cd "$SRC" && CGO_ENABLED=0 go build -o "$HOSTBIN/gdb-wui" ./cmd/gdb-wui)
    SIDECARS=$OVERLAY/usr/share/gdb-wui/sidecars
    mkdir -p "$SIDECARS"
    for f in "$OVERLAY"/root/demo/*; do
        case $f in *.c) continue ;; esac
        "$HOSTBIN/gdb-wui" -ghidra "$GHIDRA_INSTALL_DIR" -write-sidecar "$f" -decomp-sidecars "$SIDECARS"
        # The program's path is where it was here: say where it is in the
        # guest instead (it is only informational; the sha256 matches it)
        n=$(basename "$f")
        sed -i "0,/\"path\":/s|\"path\":\"[^\"]*\"|\"path\":\"/root/demo/$n\"|" "$SIDECARS/$n.decomp.json"
    done
    rm -r "${HOSTBIN:?}"
else
    echo "overlay.sh: GHIDRA_INSTALL_DIR is not set; no sidecars, so no decompilation in the guest" >&2
fi

cp "$HERE/S95gdb-wui" "$OVERLAY/etc/init.d/"
cat > "$OVERLAY/etc/motd" <<'EOF'
gdb-wui is running on this machine: its window is next to this console.
Its project is /root/demo: gdb-wui's example programs, with their sources,
and busybox. Their decompilations (the Decompiled tab) were made by Ghidra
when this image was built. To run busybox under gdb, type in gdb-wui's
console: set args ls -l /
To debug under gdbserver: gdbserver :1234 /root/demo/hello &
and click "connect" in gdb-wui's console bar.
EOF
