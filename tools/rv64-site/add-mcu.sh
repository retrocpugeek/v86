#!/bin/sh
# Adds (or replaces) a CH32V003 flash image in build/rv64-site/images/<id>/
# and lists it in images/manifest.json, e.g. the Tetris firmware:
#
#   CROSS=riscv32-buildroot-linux-gnu- tools/ch32v003/build.sh
#   tools/rv64-site/add-mcu.sh ch32v003-tetris "CH32V003 (bare metal): Tetris" build/ch32v003/tetris.bin
#
# CONTROLS=tetris (the default) shows on-screen game controls; CONTROLS=
# none doesn't. If <flash>.json (from tools/ch32v003/build.sh) exists, its
# sizes go into the manifest, for the page's resource table.
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
node - "$ROOT/build/rv64-site/images/manifest.json" "$ID" "$LABEL" "${CONTROLS-tetris}" "$STATS" <<'JS'
const fs = require("fs");
const [file, id, label, controls, stats] = process.argv.slice(2);
const m = fs.existsSync(file) ? JSON.parse(fs.readFileSync(file)) : { builds: [] };
const entry = { id, label, arch: "ch32v003", bios: "flash.bin" };
if(controls && controls !== "none") entry.controls = controls;
if(stats) entry.stats = JSON.parse(fs.readFileSync(stats));
const i = m.builds.findIndex(b => b.id === id);
if(i >= 0) m.builds[i] = entry; else m.builds.push(entry);
fs.writeFileSync(file, JSON.stringify(m, null, 2) + "\n");
console.log(`${id}: ${label}`);
JS
