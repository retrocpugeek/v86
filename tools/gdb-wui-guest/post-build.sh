#!/bin/sh
# Buildroot post-build script for the "Buildroot + gdb-wui" images: leaves out
# the files that record the build machine's paths, and stops the build if any
# other file does.
#   $1: the target directory; BUILD_DIR_PREFIX (default $HOME/): the path to look for
set -eu
T=$1
PREFIX=${BUILD_DIR_PREFIX:-$HOME/}
# socat's procan prints the compiler it was built with; libstdc++'s
# pretty-printer script needs gdb's Python, which this gdb is built without
rm -f "$T/usr/bin/procan" "$T"/usr/lib/libstdc++.so.*-gdb.py
rm -rf "$T"/usr/share/gcc-*
# The demo's busybox is the image's own, so that what it shows of it is true
# of /bin/busybox (overlay.sh strips it as Buildroot strips the target)
if [ -e "$T/root/demo/busybox" ] && ! cmp -s "$T/bin/busybox" "$T/root/demo/busybox"; then
    echo "post-build: /root/demo/busybox differs from /bin/busybox" >&2
    exit 1
fi
if grep -rl "$PREFIX" "$T" >&2; then
    echo "post-build: the files above contain $PREFIX" >&2
    exit 1
fi
