#!/bin/sh
# Builds the firmware of Vlad Tomoiagă's osillyscope
# (https://github.com/tvlad1234/osillyscope, MIT: a CH32V003 oscilloscope,
# its input on PD5 (ADC channel 5, sampled continuously into a DMA buffer
# and triggered by the analog watchdog), an SSD1306 128x64 OLED on
# bit-banged I2C (PA1 SCL, PA2 SDA) and three buttons (PD4, PC7, PC6)) from
# source, at a pinned commit with its ch32fun submodule, for the
# v86-riscv site.
#
# Compiled as ch32fun.mk compiles it for the CH32V003, without running
# the project's makefiles (Ubuntu: gcc-riscv64-unknown-elf,
# picolibc-riscv64-unknown-elf, for the C headers):
#
#   tools/ch32v003-osillyscope/build.sh [out-dir]
#
# Writes osillyscope.bin and .json (sizes, for add-mcu.sh, and where its
# menu's state is, for the page: read with gdb-multiarch) to out-dir
# (default build/ch32v003-osillyscope), then
#   BOARD=osillyscope CONTROLS=none MHZ=48 DEBUG=osillyscope \
#     tools/rv64-site/add-mcu.sh osillyscope "osillyscope: a CH32V003 oscilloscope" \
#     build/ch32v003-osillyscope/osillyscope.bin
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=$(realpath -m "${1:-$HERE/../../build/ch32v003-osillyscope}")
REPO=https://github.com/tvlad1234/osillyscope
COMMIT=5ae18b98c4f67dcee8496ba370e7ab86f718614f
INCLUDE=/usr/lib/picolibc/riscv64-unknown-elf/include
CROSS=riscv64-unknown-elf-
NAME=osillyscope
SRC=$OUT/src
mkdir -p "$OUT"
if [ ! -d "$SRC/.git" ]; then
    git clone -q "$REPO" "$SRC"
fi
git -C "$SRC" fetch -q origin "$COMMIT" 2>/dev/null || true
git -C "$SRC" checkout -q --detach "$COMMIT"
git -C "$SRC" submodule -q update --init
git -C "$SRC" clean -qfdx
FUN=$SRC/ch32fun/ch32fun
# The splash screen's build date (__DATE__): the commit's
SOURCE_DATE_EPOCH=$(git -C "$SRC" log -1 --format=%ct)
export SOURCE_DATE_EPOCH
cd "$SRC/fw"
# The debug info names the sources as /root/mcu/osillyscope, where the
# v86-riscv site's gdb-wui image has them (tools/rv64-gdb/build.sh)
FLAGS="-march=rv32ec_zicsr -mabi=ilp32e -DCH32V003 -g -Os -flto -ffunction-sections -fdata-sections -msmall-data-limit=8 -fdebug-prefix-map=$SRC=/root/mcu/$NAME"
"${CROSS}gcc" -E -P -x c -DTARGET_MCU=CH32V003 -DMCU_PACKAGE=1 -DTARGET_MCU_LD=0 "$FUN/ch32fun.ld" > "$OUT/$NAME.ld"
# (GCC 14 makes two of the warnings it gives errors: implicit declarations
# and the display's functions assigned to the drawing library's void *
# pointers)
"${CROSS}gcc" $FLAGS -Wno-error=implicit-function-declaration -Wno-error=incompatible-pointer-types -static-libgcc -isystem "$INCLUDE" -I"$FUN/../extralibs" -I"$FUN" -nostdlib -I. -Wall \
    -T "$OUT/$NAME.ld" -Wl,--gc-sections -Wl,-Map,"$OUT/$NAME.map" \
    -o "$OUT/$NAME.elf" "$FUN/ch32fun.c" osillyscope.c ssd1306.c gfx.c adc.c scope.c ui.c -lgcc
"${CROSS}objcopy" -R .storage -O binary "$OUT/$NAME.elf" "$OUT/$NAME.bin"
set -- $("${CROSS}size" "$OUT/$NAME.elf" | tail -1)
# The menu's selection and the time and volts per division chosen
addr() { gdb-multiarch -batch -ex "print/d &$1" "$OUT/$NAME.elf" | sed -n 's/^\$[0-9]* = \([0-9]*\)$/\1/p'; }
printf '{ "flash": %d, "flash_size": 16384, "text": %d, "data": %d, "bss": %d, "ram_base": %d, "ram_size": 2048, "symbols": { "ui_selector": %d, "tdiv_sel": %d, "vdiv_sel": %d } }\n' \
    "$(stat -c %s "$OUT/$NAME.bin")" "$1" "$2" "$3" 0x20000000 \
    "$(addr ui_selector)" "$(addr oscope.tdiv_sel)" "$(addr oscope.vdiv_sel)" > "$OUT/$NAME.json"
echo "$NAME: $(cat "$OUT/$NAME.json")"
