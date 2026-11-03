#!/bin/sh
# Builds basic.bin (flash image) and basic.json (its sizes) for the CH32V003:
# Tiny BASIC with a machine monitor, with tools/ch32v003's startup code,
# linker script and register definitions, and a RISC-V GCC that supports
# rv32ec/ilp32e (e.g. riscv64-unknown-elf):
#
#   tools/ch32v003-basic/build.sh [out-dir]
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=$(realpath -m "${1:-$HERE/../../build/ch32v003-basic}")
CROSS=${CROSS:-riscv64-unknown-elf-}
mkdir -p "$OUT"
# The debug info names the sources as /root/mcu/basic, where the v86-riscv
# site's gdb-wui image has them (tools/rv64-gdb/build.sh), and the
# compiler's own headers as under /usr (built from tools, so that the build
# directory is the sources')
TOOLCHAIN=$(realpath "$(dirname "$(command -v "${CROSS}gcc")")/..")
TOOLS=$(cd "$HERE/.." && pwd)
cd "$TOOLS"
"${CROSS}gcc" -march=rv32ec_zicsr -mabi=ilp32e -Os -g -fdebug-prefix-map="$TOOLCHAIN=/usr" -fdebug-prefix-map="$TOOLS=/root/mcu/basic" \
  -ffreestanding -nostdlib -fno-builtin -fno-stack-protector -fno-pie -no-pie -static \
  -ffunction-sections -fdata-sections -Wall -Wextra -I ch32v003 \
  -T ch32v003/link.ld -Wl,--gc-sections -Wl,-Map,"$OUT/basic.map" \
  -o "$OUT/basic.elf" ch32v003/start.S ch32v003-basic/basic.c ch32v003/libgcc.c
"${CROSS}objcopy" -O binary "$OUT/basic.elf" "$OUT/basic.bin"
"${CROSS}size" "$OUT/basic.elf"
# The last 1 KB of flash is where SAVE puts the program
SIZE=$(stat -c %s "$OUT/basic.bin")
[ "$SIZE" -le 15360 ] || { echo "basic.bin is $SIZE bytes: it runs into SAVE's 1 KB at 0x3c00" >&2; exit 1; }
# Sizes for the v86-riscv site (add-mcu.sh puts them in its manifest)
set -- $("${CROSS}size" "$OUT/basic.elf" | tail -1)
sym() { "${CROSS}nm" "$OUT/basic.elf" | awk -v s="$1" '$3 == s { print "0x" $1 }'; }
printf '{ "flash": %d, "flash_size": 16384, "text": %d, "data": %d, "bss": %d, "ram_base": %d, "ram_size": 2048, "stack_bottom": %d, "stack_top": %d, "stack_fill": 2779096485 }\n' \
  "$SIZE" "$1" "$2" "$3" 0x20000000 "$(sym _ebss)" "$(sym _estack)" > "$OUT/basic.json"
cat "$OUT/basic.json"
