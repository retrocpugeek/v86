#!/bin/sh
# Buildroot post-build script: drop files that embed the build machine's
# paths (GCC's GDB pretty-printer loaders).
set -eu
TARGET=$1
find "$TARGET/usr/lib" -name '*-gdb.py' -delete
rm -rf "$TARGET"/usr/share/gcc-*
