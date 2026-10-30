#!/bin/sh
# Builds the firmware of Maksym Curul's CH32V003 Mini Game Console
# (https://github.com/max7d3/CH32V003-MiniGameConsole, Apache-2.0: a
# CH32V003F4P6, an ST7735 80x160 IPS LCD on SPI, five buttons, a PWM buzzer;
# Snake, Pong, Jump Bird and Dino Run) from source, at a pinned commit, for
# the v86-riscv site.
#
# Its MounRiver Studio project is built as that IDE would (-Os, LTO,
# sections garbage-collected, Ld/Link.ld, no start files), with upstream
# GCC instead of WCH's (Ubuntu: gcc-riscv64-unknown-elf,
# picolibc-riscv64-unknown-elf, for the C headers):
# - WCH's interrupt("WCH-Interrupt-fast") is plain interrupt: handlers save
#   the registers themselves, and the chip's hardware stacking (on) is
#   restored by mret either way
# - mini-libc.c has the few C library functions the firmware uses (newlib
#   in the IDE), and stddef.h is included for debug.c's _sbrk
#
#   tools/ch32v003-minigameconsole/build.sh [out-dir]
#
# Writes minigameconsole.bin and .json (sizes, for add-mcu.sh) to out-dir
# (default build/ch32v003-minigameconsole), then
#   BOARD=minigameconsole CONTROLS=none MHZ=48 DEBUG=minigameconsole \
#     tools/rv64-site/add-mcu.sh minigameconsole "Mini game console: Snake, Pong, Jump Bird, Dino Run" \
#     build/ch32v003-minigameconsole/minigameconsole.bin
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=$(realpath -m "${1:-$HERE/../../build/ch32v003-minigameconsole}")
REPO=https://github.com/max7d3/CH32V003-MiniGameConsole
COMMIT=edf73145d6e4cf39626b2fc60adab37fad4f8f0f
INCLUDE=/usr/lib/picolibc/riscv64-unknown-elf/include
CROSS=riscv64-unknown-elf-
NAME=minigameconsole
SRC=$OUT/src
mkdir -p "$OUT"
if [ ! -d "$SRC/.git" ]; then
    git clone -q "$REPO" "$SRC"
fi
git -C "$SRC" fetch -q origin "$COMMIT" 2>/dev/null || true
git -C "$SRC" checkout -q --detach "$COMMIT"
git -C "$SRC" clean -qfdx
dir=$SRC/Software/TinyGameConsole
cp "$HERE/mini-libc.c" "$dir/User/"
cd "$dir"
# The debug info names the sources as /root/mcu/minigameconsole, where the
# v86-riscv site's gdb-wui image has them (tools/rv64-gdb/build.sh)
FLAGS="-march=rv32ec_zicsr -mabi=ilp32e -Os -flto -g -ffunction-sections -fdata-sections -fno-common -fdebug-prefix-map=$dir=/root/mcu/$NAME"
"${CROSS}gcc" $FLAGS -c Startup/startup_ch32v00x.S -o "$OUT/startup.o"
# shellcheck disable=SC2046
"${CROSS}gcc" $FLAGS -std=gnu99 -fsigned-char -include stddef.h '-Dinterrupt(x)=interrupt' \
    -ICore -IDebug -IUser -IPeripheral/inc -isystem "$INCLUDE" \
    -nostartfiles -nostdlib -T Ld/Link.ld -Wl,--gc-sections -Wl,-Map,"$OUT/$NAME.map" \
    -o "$OUT/$NAME.elf" "$OUT/startup.o" $(ls Core/*.c Debug/*.c Peripheral/src/*.c User/*.c User/games/*.c) -lgcc
rm "$OUT/startup.o"
"${CROSS}objcopy" -O binary "$OUT/$NAME.elf" "$OUT/$NAME.bin"
set -- $("${CROSS}size" "$OUT/$NAME.elf" | tail -1)
printf '{ "flash": %d, "flash_size": 16384, "text": %d, "data": %d, "bss": %d, "ram_base": %d, "ram_size": 2048 }\n' \
    "$(stat -c %s "$OUT/$NAME.bin")" "$1" "$2" "$3" 0x20000000 > "$OUT/$NAME.json"
echo "$NAME: $(cat "$OUT/$NAME.json")"
