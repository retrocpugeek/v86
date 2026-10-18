#!/bin/sh
# Builds tetris.bin (flash image) and tetris.json (its sizes) for the CH32V003 with a RISC-V GCC that
# supports rv32ec/ilp32e, e.g. Buildroot's riscv32 toolchain or
# riscv64-unknown-elf:
#
#   CROSS=riscv32-buildroot-linux-gnu- tools/ch32v003/build.sh [out-dir]
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${1:-$HERE/../../build/ch32v003}
CROSS=${CROSS:-riscv64-unknown-elf-}
mkdir -p "$OUT"
"${CROSS}gcc" -march=rv32ec_zicsr -mabi=ilp32e -Os -g -ffreestanding -nostdlib -fno-builtin -fno-stack-protector -fno-pie -no-pie -static \
  -ffunction-sections -fdata-sections -Wall -Wextra \
  -T "$HERE/link.ld" -Wl,--gc-sections -Wl,-Map,"$OUT/tetris.map" \
  -o "$OUT/tetris.elf" "$HERE/start.S" "$HERE/tetris.c" "$HERE/libgcc.c"
"${CROSS}objcopy" -O binary "$OUT/tetris.elf" "$OUT/tetris.bin"
"${CROSS}size" "$OUT/tetris.elf"
# Sizes for the v86-riscv site (add-mcu.sh puts them in its manifest)
set -- $("${CROSS}size" "$OUT/tetris.elf" | tail -1)
sym() { "${CROSS}nm" "$OUT/tetris.elf" | awk -v s="$1" '$3 == s { print "0x" $1 }'; }
printf '{ "flash": %d, "flash_size": 16384, "text": %d, "data": %d, "bss": %d, "ram_base": %d, "ram_size": 2048, "stack_bottom": %d, "stack_top": %d, "stack_fill": 2779096485 }\n' \
  "$(stat -c %s "$OUT/tetris.bin")" "$1" "$2" "$3" 0x20000000 "$(sym _ebss)" "$(sym _estack)" > "$OUT/tetris.json"
cat "$OUT/tetris.json"
