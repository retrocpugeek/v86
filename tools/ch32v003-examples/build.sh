#!/bin/sh
# Builds the CH32V003 firmware in tools/ch32v003-examples/firmware, which the
# v86-riscv site's "Run your own CH32V003 firmware" page (?build=upload)
# offers as examples, from GitHub (raw.githubusercontent.com):
#
#  - examples of ch32fun (https://github.com/cnlohr/ch32fun, MIT), at a
#    pinned commit, compiled as ch32fun.mk compiles them for the CH32V003,
#    without running its makefiles (Ubuntu: gcc-riscv64-unknown-elf,
#    picolibc-riscv64-unknown-elf, for the C headers);
#  - the site's other CH32V003 firmware, as their own build scripts built it
#    (tools/ch32v003, tools/ch32v003-minigameconsole,
#    tools/ch32v003-osillyscope), with their licences.
#
#   tools/ch32v003-examples/build.sh
#
# ELFs, with their symbols and line numbers (the debug info names ch32fun's
# sources as /ch32fun/...), so that gdb-wui has them on the page's ?debug.
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
OUT=$HERE/firmware
WORK=$ROOT/build/ch32v003-examples
REPO=https://github.com/cnlohr/ch32fun
COMMIT=efee141f8d25aaed03509272a1180269172f4323
INCLUDE=/usr/lib/picolibc/riscv64-unknown-elf/include
CROSS=riscv64-unknown-elf-
# The examples that run on the emulated CH32V003 and show something: on
# the serial console (printf, through USART1 or ch32fun's debug printf) or
# the game console's OLED (i2c_oled); tim1_pwm shows the page's note on what
# isn't modelled (TIM1)
EXAMPLES="debugprintfdemo uartdemo systick_irq template random_numbers flashtest hsitrim optiondata tim1_pwm i2c_oled"
SRC=$WORK/ch32fun
mkdir -p "$OUT" "$WORK"
if [ ! -d "$SRC/.git" ]; then
    git clone -q "$REPO" "$SRC"
fi
git -C "$SRC" fetch -q origin "$COMMIT" 2>/dev/null || true
git -C "$SRC" checkout -q --detach "$COMMIT"
git -C "$SRC" clean -qfdx
FUN=$SRC/ch32fun
SOURCE_DATE_EPOCH=$(git -C "$SRC" log -1 --format=%ct)
export SOURCE_DATE_EPOCH
# picolibc's stdio.h makes putchar a macro, which ch32fun.c defines as a
# function
mkdir -p "$WORK/shim"
printf '#include_next <stdio.h>\n#undef putchar\n' > "$WORK/shim/stdio.h"
"${CROSS}gcc" -E -P -x c -DTARGET_MCU=CH32V003 -DMCU_PACKAGE=1 -DTARGET_MCU_LD=0 "$FUN/ch32fun.ld" > "$WORK/ch32v003.ld"
FLAGS="-march=rv32ec_zicsr -mabi=ilp32e -DCH32V003 -g -Os -flto -ffunction-sections -fdata-sections -msmall-data-limit=8 -fdebug-prefix-map=$SRC=/ch32fun"
# (from the checkout, so that the debug info's compile directory is
# /ch32fun too)
cd "$SRC"
for ex in $EXAMPLES; do
    d=$SRC/examples/$ex
    "${CROSS}gcc" $FLAGS -static-libgcc -I"$WORK/shim" -isystem "$INCLUDE" -I"$FUN/../extralibs" -I"$FUN" -I"$d" -nostdlib \
        -T "$WORK/ch32v003.ld" -Wl,--gc-sections -o "$OUT/$ex.elf" "$FUN/ch32fun.c" "$d"/*.c -lgcc
    echo "$ex: $(stat -c %s "$OUT/$ex.elf") bytes"
done
cp "$SRC/LICENSE" "$OUT/LICENSE-ch32fun"

# The site's own builds, where they have been built
copy() {
    [ -f "$ROOT/build/$1/$2.elf" ] || { echo "$2: not built (tools/$1/build.sh)"; return; }
    cp "$ROOT/build/$1/$2.elf" "$OUT/$2.elf"
    echo "$2: $(stat -c %s "$OUT/$2.elf") bytes"
}
copy ch32v003 tetris
copy ch32v003-minigameconsole minigameconsole
copy ch32v003-osillyscope osillyscope
[ -f "$ROOT/build/ch32v003-minigameconsole/src/LICENSE" ] && cp "$ROOT/build/ch32v003-minigameconsole/src/LICENSE" "$OUT/LICENSE-minigameconsole"
[ -f "$ROOT/build/ch32v003-osillyscope/src/LICENSE" ] && cp "$ROOT/build/ch32v003-osillyscope/src/LICENSE" "$OUT/LICENSE-osillyscope"
# Nothing of this computer's in them
if grep -l -a "$HOME" "$OUT"/*.elf; then
    echo "these name $HOME" >&2
    exit 1
fi
