#!/bin/sh
# Builds tetris.bin (flash image) for the CH32V003 with a RISC-V GCC that
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
