#!/bin/sh
# Adds (or replaces) a CH32V microcontroller flash image in
# build/rv64-site/images/<id>/ and lists it in images/manifest.json, e.g.:
#
#   CROSS=riscv32-buildroot-linux-gnu- tools/ch32v003/build.sh
#   DEBUG=tetris tools/rv64-site/add-mcu.sh ch32v003-tetris "CH32V003 (bare metal): Tetris" build/ch32v003/tetris.bin
#
#   tools/ch32v307/build.sh
#   ARCH=ch32v307 CONTROLS=shell COLS=80 ROWS=28 LEDS=LED1:1:5 \
#     COMMANDS="help,ps,free,date,aclock,snake,lua,core_mark,clear" \
#     tools/rv64-site/add-mcu.sh ch32v307-rtthread "CH32V307: RT-Thread shell" build/ch32v307/rtthread.bin
#
#   tools/ch32v003-gameconsole/build.sh
#   BOARD=gameconsole CONTROLS=none MHZ=12 DEBUG=tiny_tris GROUP="CH32V003 game console" \
#     tools/rv64-site/add-mcu.sh gameconsole-tris "CH32V003 game console: Tiny Tris" build/ch32v003-gameconsole/tiny_tris.bin
#
#   tools/ch32v003-minigameconsole/build.sh
#   BOARD=minigameconsole CONTROLS=none MHZ=48 DEBUG=minigameconsole LEDS=PWR:3:0 \
#     tools/rv64-site/add-mcu.sh minigameconsole "Mini game console: Snake, Pong, Jump Bird, Dino Run" \
#     build/ch32v003-minigameconsole/minigameconsole.bin
#
#   tools/ch32v003-osillyscope/build.sh
#   BOARD=osillyscope CONTROLS=none MHZ=48 \
#     tools/rv64-site/add-mcu.sh osillyscope "osillyscope: a CH32V003 oscilloscope" \
#     build/ch32v003-osillyscope/osillyscope.bin
#
# ARCH: ch32v003 (default) or ch32v307. CONTROLS: tetris (the default) for
# on-screen game controls, shell for shell keys and the COMMANDS buttons, or
# none. COLS x ROWS: the terminal (default 46 x 25). LEDS: label:port:pin,...
# (port 0 is GPIOA) shown from the GPIO outputs. If <flash>.json (from the
# build scripts) exists, its sizes go into the manifest, for the page's
# resource table. BOARD=gameconsole: the CH32V003 game console (an OLED
# display, buttons and a buzzer instead of a terminal); minigameconsole: the
# mini game console (a colour LCD, buttons and a buzzer); osillyscope: the
# oscilloscope (an OLED, buttons, and its input from the page); MHZ: the clock the
# firmware sets, for the page. DEBUG: the firmware's project in the gdb-wui
# image (/root/mcu/<DEBUG>, tools/rv64-gdb/build.sh), e.g. tetris or
# tiny_tris: the page offers to debug it (?debug) with that image. GROUP:
# builds with the same GROUP are one entry in the page's image list, and a
# second list picks among them (by their labels, less "<GROUP>: ").
set -eu
[ $# -eq 3 ] || { echo "usage: $0 <id> <label> <flash.bin>" >&2; exit 1; }
ID=$1 LABEL=$2 FLASH=$3
case "$ID" in *[!a-z0-9._-]*|"") echo "bad id: $ID" >&2; exit 1;; esac
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
DIR=$ROOT/build/rv64-site/images/$ID
rm -rf "$DIR"
mkdir -p "$DIR"
cp -L "$FLASH" "$DIR/flash.bin"
STATS=${FLASH%.bin}.json
[ -f "$STATS" ] || STATS=
node - "$ROOT/build/rv64-site/images/manifest.json" "$ID" "$LABEL" "${CONTROLS-tetris}" "$STATS" \
  "${ARCH:-ch32v003}" "${COLS:-}" "${ROWS:-}" "${LEDS:-}" "${COMMANDS:-}" "${BOARD:-}" "${MHZ:-}" "${DEBUG:-}" "${GROUP:-}" <<'JS'
const fs = require("fs");
const [file, id, label, controls, stats, arch, cols, rows, leds, commands, board, mhz, debug, group] = process.argv.slice(2);
const m = fs.existsSync(file) ? JSON.parse(fs.readFileSync(file)) : { builds: [] };
const entry = { id, label, arch, bios: "flash.bin" };
if(controls && controls !== "none") entry.controls = controls;
if(cols) entry.cols = +cols;
if(rows) entry.rows = +rows;
if(leds) entry.leds = leds.split(",").map(l => { const [label, port, pin] = l.split(":"); return { label, port: +port, pin: +pin }; });
if(commands) entry.commands = commands.split(",");
if(board) entry.board = board;
if(mhz) entry.mhz = +mhz;
if(debug) entry.debug = debug;
if(group) entry.group = group;
if(stats) entry.stats = JSON.parse(fs.readFileSync(stats));
const i = m.builds.findIndex(b => b.id === id);
if(i >= 0) m.builds[i] = entry; else m.builds.push(entry);
fs.writeFileSync(file, JSON.stringify(m, null, 2) + "\n");
console.log(`${id}: ${label}`);
JS
# Into a directory named after its contents (see the script)
python3 "$ROOT/tools/version-images.py" "$ROOT/build/rv64-site"
