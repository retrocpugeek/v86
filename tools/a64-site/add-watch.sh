#!/bin/sh
# Adds (or replaces) a Pebble watch in build/a64-site/images/<id>/ and lists it
# in images/manifest.json: PebbleOS for an emulator board (pebble/build.sh),
# run by v86's Cortex-M core (src/cm) in the page's watch.
#
#   tools/a64-site/add-watch.sh <id> <label> <flint|emery|gabbro> <dir with firmware.elf and spi.bin.gz>
set -eu
[ $# -eq 4 ] || { echo "usage: $0 <id> <label> <board> <dir>" >&2; exit 1; }
ID=$1 LABEL=$2 BOARD=$3 SRC=$4
case "$ID" in *[!a-z0-9._-]*|"") echo "bad id: $ID" >&2; exit 1;; esac
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
DIR=$ROOT/build/a64-site/images/$ID
rm -rf "${DIR:?}"
mkdir -p "$DIR"
cp "$SRC/firmware.elf" "$SRC/spi.bin.gz" "$DIR/"
VERSION=$(cat "$SRC/version.txt" 2>/dev/null || echo "")
node - "$ROOT/build/a64-site/images/manifest.json" "$ID" "$LABEL" "$BOARD" "$VERSION" <<'JS'
const fs = require("fs");
const [file, id, label, watch, version] = process.argv.slice(2);
const m = fs.existsSync(file) ? JSON.parse(fs.readFileSync(file)) : { builds: [] };
const entry = {
    id, label, watch, firmware: "firmware.elf", spi: "spi.bin.gz",
    about: `<a href="https://github.com/coredevices/PebbleOS">PebbleOS</a> (Apache-2.0)` +
        (version ? ` ${version.replace(/[<&]/g, "")}` : "") + `, built for its <code>qemu_${watch}</code> emulator board ` +
        `(<a href="https://github.com/retrocpugeek/v86/tree/ports/tools/a64-site/pebble">build script</a>), running on v86's ` +
        `Cortex-M core in your browser. Its machine is the one PebbleOS's own emulator provides: a framebuffer display, ` +
        `four buttons, simple UARTs, timers, an RTC and external flash. The clock shows UTC; there's no phone to connect yet.`,
};
const i = m.builds.findIndex(b => b.id === id);
if(i >= 0) m.builds[i] = entry; else m.builds.push(entry);
fs.writeFileSync(file, JSON.stringify(m, null, 2) + "\n");
console.log(`${id}: ${label}`);
JS
python3 "$ROOT/tools/version-images.py" "$ROOT/build/a64-site"
