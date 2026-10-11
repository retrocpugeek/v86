#!/bin/sh
set -e
cd "$(dirname "$0")"
aarch64-linux-gnu-gcc -O2 -ffreestanding -nostdlib -static -mgeneral-regs-only -Wl,-Ttext=0x40080000 -Wl,--build-id=none -o idprobe.elf idprobe.c
