#!/bin/sh
# Buildroot post-build script for the gdb-wui image (tools/rv64-gdb/build.sh):
# leaves out the files that record the build machine's paths, and stops the
# build if any other file does.
#   $1: the target directory; BUILD_DIR_PREFIX: the path to look for
set -eu
T=$1
# socat's procan prints the compiler it was built with; libstdc++'s
# pretty-printer script needs gdb's Python, which this gdb is built without
rm -f "$T/usr/bin/procan" "$T"/usr/lib/libstdc++.so.*-gdb.py
if grep -rl "$BUILD_DIR_PREFIX" "$T" >&2; then
    echo "post-build: the files above contain $BUILD_DIR_PREFIX" >&2
    exit 1
fi
