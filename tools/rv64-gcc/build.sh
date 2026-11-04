#!/bin/sh
# Builds GCC and binutils that run on riscv64 Linux (a "Canadian cross",
# with Buildroot's cross toolchain), for the v86-riscv site's gdb-wui image:
# with them, the microcontroller firmware in /root/mcu can be rebuilt in the
# guest. One GCC targets both the guest and the CH32V003 (-march=rv32ec_zicsr
# -mabi=ilp32e): the firmware brings its own startup code and libgcc.c, so
# no C library or target libgcc is needed, and C alone, without LTO, is built.
#
#   tools/rv64-gcc/build.sh BUILDROOT_TREE
#
# BUILDROOT_TREE: the gdb-wui image's tree (see tools/rv64-gdb/build.sh),
# whose out/host has the cross toolchain. Sources come from Buildroot's
# download dir (BR2_DL_DIR). Built in Buildroot's container image.
#
# Output: build/rv64-gcc/root (usr/bin/{gcc,as,ld,...}, usr/libexec/gcc/...),
# stripped, which tools/rv64-gdb/build.sh copies into the image's overlay.
set -eu
TREE=$(realpath "$1")
cd "$(dirname "$0")/../.."
ROOT=$PWD
OUT=$ROOT/build/rv64-gcc
DL=${BR2_DL_DIR:-$HOME/.local/share/brss/dl}
IMAGE=registry.gitlab.com/buildroot.org/buildroot/base:20250218.2110
JOBS=${JOBS:-4}
GCC=gcc-14.3.0 BINUTILS=binutils-2.44 GMP=gmp-6.3.0 MPFR=mpfr-4.1.1 MPC=mpc-1.3.1
mkdir -p "$OUT/src"
cd "$OUT/src"
[ -d $BINUTILS ] || tar xf "$DL/binutils/$BINUTILS.tar.xz"
if [ ! -d $GCC ]; then
    tar xf "$DL/gcc/$GCC.tar.xz"
    # built in GCC's tree, and linked in statically
    tar xf "$DL/gmp/$GMP.tar.xz" && mv $GMP $GCC/gmp
    tar xf "$DL/mpfr/$MPFR.tar.xz" && mv $MPFR $GCC/mpfr
    tar xzf "$DL/mpc/$MPC.tar.gz" && mv $MPC $GCC/mpc
fi
cd "$ROOT"

# In the container, the tree is /tree and the work is /work, so that no
# path of this machine's gets into the programs
cat > "$OUT/inside.sh" <<EOF
set -eu
export PATH=/tree/out/host/bin:\$PATH
T=riscv64-buildroot-linux-gnu
SYSROOT=/tree/out/host/\$T/sysroot
FLAGS="-O2 -ffile-prefix-map=/work=/usr/src"
rm -rf /work/root /work/b-binutils /work/b-gcc
mkdir -p /work/b-binutils /work/b-gcc
cd /work/b-binutils
CFLAGS="\$FLAGS" CXXFLAGS="\$FLAGS" ../src/$BINUTILS/configure --build=x86_64-pc-linux-gnu --host=\$T --target=\$T \\
    --prefix=/usr --disable-nls --disable-werror --disable-gdb --disable-gdbserver --disable-sim --disable-gprofng \\
    --disable-gold --disable-plugins --disable-shared --enable-static --without-zstd --with-sysroot=/ >/work/binutils.log 2>&1
make -j$JOBS >>/work/binutils.log 2>&1
make DESTDIR=/work/root install-strip >>/work/binutils.log 2>&1
cd /work/b-gcc
CFLAGS="\$FLAGS" CXXFLAGS="\$FLAGS" ../src/$GCC/configure --build=x86_64-pc-linux-gnu --host=\$T --target=\$T \\
    --prefix=/usr --with-build-sysroot=\$SYSROOT --with-sysroot=/ --with-native-system-header-dir=/usr/include \\
    --with-arch=rv64gc --with-abi=lp64d --enable-languages=c --disable-lto --disable-plugin \\
    --disable-multilib --disable-nls --disable-libsanitizer --disable-libgomp --disable-libquadmath \\
    --disable-libssp --disable-libatomic --disable-libitm --disable-libvtv --disable-bootstrap \\
    --without-isl --without-zstd >/work/gcc.log 2>&1
make -j$JOBS all-gcc >>/work/gcc.log 2>&1
make DESTDIR=/work/root install-strip-gcc >>/work/gcc.log 2>&1
EOF

podman run --rm --userns=keep-id -v "$TREE:/tree:z" -v "$OUT:/work:z" -e HOME=/tmp -w /work "$IMAGE" sh /work/inside.sh

# Only what compiling C and linking takes; no docs, no headers of GCC's
# plugins, no target libraries
R=$OUT/root/usr
# (lib/gcc/.../include stays: stdint.h and the like, for freestanding C)
rm -rf "${R:?}/share" "${R:?}/include"
rm -rf "$R"/libexec/gcc/*/*/plugin "$R"/libexec/gcc/*/*/install-tools "$R"/lib/gcc/*/*/install-tools
find "$R/bin" -name 'riscv64-buildroot-linux-gnu-*' -delete
rm -f "$R/bin/gcov" "$R/bin/gcov-dump" "$R/bin/gcov-tool" "$R/bin/lto-dump" "$R"/libexec/gcc/*/*/lto-wrapper
# binutils' libraries, and its programs that building firmware doesn't use
# (it keeps as, ld, nm, objcopy, objdump, readelf, size, strip)
rm -f "$R"/lib/*.a "$R"/lib/*.la
for p in addr2line ar c++filt elfedit gcc-ar gcc-nm gcc-ranlib gprof ranlib strings; do
    rm -f "$R/bin/$p" "$R/riscv64-buildroot-linux-gnu/bin/$p"
done
if grep -rl --binary-files=binary "$HOME" "$OUT/root"; then
    echo "these name $HOME" >&2
    exit 1
fi
du -sh "$OUT/root"
find "$OUT/root" -type f | sort
