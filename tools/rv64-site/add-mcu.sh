#!/bin/sh
# Adds (or replaces) a CH32V microcontroller flash image in
# build/rv64-site/images/<id>/ and lists it in images/manifest.json, e.g.:
#
#   CROSS=riscv32-buildroot-linux-gnu- tools/ch32v003/build.sh
#   tools/rv64-site/add-mcu.sh ch32v003-tetris "CH32V003 (bare metal): Tetris" build/ch32v003/tetris.bin
#
#   tools/ch32v307/build.sh
#   ARCH=ch32v307 CONTROLS=shell COLS=80 ROWS=28 LEDS=LED1:1:5 \
#     COMMANDS="help,ps,free,date,aclock,snake,lua,core_mark,clear" \
#     tools/rv64-site/add-mcu.sh ch32v307-rtthread "CH32V307: RT-Thread shell" build/ch32v307/rtthread.bin
#
# ARCH: ch32v003 (default) or ch32v307. CONTROLS: tetris (the default) for
# on-screen game controls, shell for shell keys and the COMMANDS buttons, or
# none. COLS x ROWS: the terminal (default 46 x 25). LEDS: label:port:pin,...
# (port 0 is GPIOA) shown from the GPIO outputs. If <flash>.json (from the
# build scripts) exists, its sizes go into the manifest, for the page's
# resource table.
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
  "${ARCH:-ch32v003}" "${COLS:-}" "${ROWS:-}" "${LEDS:-}" "${COMMANDS:-}" <<'JS'
const fs = require("fs");
const [file, id, label, controls, stats, arch, cols, rows, leds, commands] = process.argv.slice(2);
const m = fs.existsSync(file) ? JSON.parse(fs.readFileSync(file)) : { builds: [] };
const entry = { id, label, arch, bios: "flash.bin" };
if(controls && controls !== "none") entry.controls = controls;
if(cols) entry.cols = +cols;
if(rows) entry.rows = +rows;
if(leds) entry.leds = leds.split(",").map(l => { const [label, port, pin] = l.split(":"); return { label, port: +port, pin: +pin }; });
if(commands) entry.commands = commands.split(",");
if(stats) entry.stats = JSON.parse(fs.readFileSync(stats));
const i = m.builds.findIndex(b => b.id === id);
if(i >= 0) m.builds[i] = entry; else m.builds.push(entry);
fs.writeFileSync(file, JSON.stringify(m, null, 2) + "\n");
console.log(`${id}: ${label}`);
JS
