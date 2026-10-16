#!/bin/sh
# Buildroot post-build script: drop files that embed the build machine's
# paths (GCC's GDB pretty-printer loaders).
set -eu
TARGET=$1
find "$TARGET/usr/lib" -name '*-gdb.py' -delete
rm -rf "$TARGET"/usr/share/gcc-*

# udev is here only because Buildroot builds X's evdev driver with it. Nothing
# needs the daemon (devtmpfs makes the device nodes; xorg.conf lists the
# input devices), and its coldplug takes over 30 s on a phone.
rm -f "$TARGET/etc/init.d/S10udevd"
