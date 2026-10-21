#!/bin/sh
# Builds the games of Stefan Wagner's CH32V003 RISC-V Mini Game Console
# (https://github.com/wagiminator/CH32V003-GameConsole: a CH32V003J4M6, an
# SSD1306 128x64 OLED on I2C, five buttons, a buzzer) from source, at a
# pinned commit, for the v86-riscv site. Each game's makefile is used as it
# is, with picolibc's headers (Ubuntu: gcc-riscv64-unknown-elf,
# picolibc-riscv64-unknown-elf) and mini-libc.c.
#
#   tools/ch32v003-gameconsole/build.sh [out-dir]
#
# Writes <game>.bin and <game>.json (sizes, for add-mcu.sh) to out-dir
# (default build/ch32v003-gameconsole), then e.g.
#   ARCH=ch32v003 BOARD=gameconsole CONTROLS=gameconsole \
#     tools/rv64-site/add-mcu.sh gameconsole-tris "..." build/ch32v003-gameconsole/tiny_tris.bin
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=$(realpath -m "${1:-$HERE/../../build/ch32v003-gameconsole}")
REPO=https://github.com/wagiminator/CH32V003-GameConsole
COMMIT=ad10df94a7571768d658b03238379f99f0a3f061
GAMES="tiny_tris tiny_invaders tiny_arkanoid tiny_pacman tiny_lander tiny_conway"
INCLUDE=/usr/lib/picolibc/riscv64-unknown-elf/include
CROSS=riscv64-unknown-elf-

SRC=$OUT/src
if [ ! -d "$SRC/.git" ]; then
    git clone -q "$REPO" "$SRC"
fi
git -C "$SRC" fetch -q origin "$COMMIT" 2>/dev/null || true
git -C "$SRC" checkout -q --detach "$COMMIT"
git -C "$SRC" clean -qfdx

for game in $GAMES; do
    dir=$SRC/software/$game
    cp "$HERE/mini-libc.c" "$dir/"
    rm -f "$dir/bin/$game.bin" "$dir/bin/$game.hex"
    make -s -C "$dir" bin NEWLIB="$INCLUDE"
    cp "$dir/bin/$game.bin" "$OUT/"
    # (the makefile removes the ELF; build it again for its sizes)
    make -s -C "$dir" "bin/$game.elf" NEWLIB="$INCLUDE"
    set -- $("${CROSS}size" "$dir/bin/$game.elf" | tail -1)
    printf '{ "flash": %d, "flash_size": 16384, "text": %d, "data": %d, "bss": %d, "ram_base": %d, "ram_size": 2048 }\n' \
        "$(stat -c %s "$OUT/$game.bin")" "$1" "$2" "$3" 0x20000000 > "$OUT/$game.json"
    echo "$game: $(cat "$OUT/$game.json")"
done
