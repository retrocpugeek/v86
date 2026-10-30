#!/bin/sh
# Adds (or replaces) the card10 badge in build/a64-site/images/<id>/ and lists
# it in images/manifest.json: its firmware (card10/build.sh), run by v86's
# Cortex-M core (src/cm/src/dev/max32665.rs) in the page's badge.
#
#   tools/a64-site/add-card10.sh <id> <label> <dir with epicardium.elf, pycardium.elf, fat.img.gz, api.json>
set -eu
[ $# -eq 3 ] || { echo "usage: $0 <id> <label> <dir>" >&2; exit 1; }
ID=$1 LABEL=$2 SRC=$3
case "$ID" in *[!a-z0-9._-]*|"") echo "bad id: $ID" >&2; exit 1;; esac
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
DIR=$ROOT/build/a64-site/images/$ID
rm -rf "${DIR:?}"
mkdir -p "$DIR"
cp "$SRC/epicardium.elf" "$SRC/pycardium.elf" "$SRC/fat.img.gz" "$SRC/api.json" "$DIR/"
VERSION=$(cat "$SRC/version.txt" 2>/dev/null || echo "")
node - "$ROOT/build/a64-site/images/manifest.json" "$ID" "$LABEL" "$VERSION" <<'JS'
const fs = require("fs");
const [file, id, label, version] = process.argv.slice(2);
const m = fs.existsSync(file) ? JSON.parse(fs.readFileSync(file)) : { builds: [] };
const entry = {
    id, label, card10: true, firmware: "epicardium.elf", core1: "pycardium.elf", spi: "fat.img.gz", api: "api.json",
    about: `The <a href="https://card10.badge.events.ccc.de/">card10</a> badge from the Chaos Communication Camp 2019, ` +
        `running its <a href="https://git.flow3r.garden/card10/firmware">firmware</a> (MIT, unless a file says otherwise)` +
        (version ? ` ${version.replace(/[<&]/g, "")}` : "") +
        ` (<a href="https://github.com/retrocpugeek/v86/tree/ports/tools/a64-site/card10">build script</a>) on v86's ` +
        `Cortex-M core in your browser. Its MAX32666 has two Cortex-M4F cores: Epicardium, on FreeRTOS, runs the ` +
        `hardware on core 0; Pycardium, MicroPython, runs the apps on core 1 and calls Epicardium through memory ` +
        `both cores share, guarded by the chip's hardware semaphores, each core waking the other with an event. ` +
        `Emulated: both cores (with their FPUs), the display, the four buttons, the power management chip, the ` +
        `flash with its file system (the firmware's own apps), timers, the clock and the serial console (MicroPython's ` +
        `REPL); not yet the sensors, LEDs, vibration motor or Bluetooth.`,
};
const i = m.builds.findIndex(b => b.id === id);
if(i >= 0) m.builds[i] = entry; else m.builds.push(entry);
fs.writeFileSync(file, JSON.stringify(m, null, 2) + "\n");
console.log(`${id}: ${label}`);
JS
python3 "$ROOT/tools/version-images.py" "$ROOT/build/a64-site"
