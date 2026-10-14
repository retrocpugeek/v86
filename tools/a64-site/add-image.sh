#!/bin/sh
# Adds (or replaces) a kernel + rootfs in build/a64-site/images/<id>/ and lists
# it in images/manifest.json. The rootfs is split into 1 MB parts, which the
# page loads on demand (Cloudflare's static assets allow at most 25 MiB per file).
#
#   tools/a64-site/add-image.sh <id> <label> <Image> <rootfs.ext4|rootfs.squashfs> [cmdline]
#
# WEB=<path> (e.g. WEB=/cgi-bin/luci/) shows the guest's web interface at that
# path next to the console (the page only serves /cgi-bin/ from the guest).
set -eu
[ $# -ge 4 ] || { echo "usage: $0 <id> <label> <Image> <rootfs> [cmdline]" >&2; exit 1; }
ID=$1 LABEL=$2 KERNEL=$3 ROOTFS=$4
CMDLINE=${5:-"rootwait root=/dev/vda console=ttyAMA0"}
case "$ID" in *[!a-z0-9._-]*|"") echo "bad id: $ID" >&2; exit 1;; esac
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
DIR=$ROOT/build/a64-site/images/$ID
CHUNK=1048576
EXT=.${ROOTFS##*.}

rm -rf "$DIR"
mkdir -p "$DIR/rootfs"
cp -L "$KERNEL" "$DIR/Image"
python3 "$ROOT/tools/split-image.py" $CHUNK "$ROOTFS" "$DIR/rootfs/%d-%d$EXT"
# split-image.py pads the last part; the disk size is rounded up to match
SIZE=$(( ( $(stat -Lc %s "$ROOTFS") + CHUNK - 1 ) / CHUNK * CHUNK ))

node - "$ROOT/build/a64-site/images/manifest.json" "$ID" "$LABEL" "$EXT" "$SIZE" "$CHUNK" "$CMDLINE" "${WEB:-}" <<'JS'
const fs = require("fs");
const [file, id, label, ext, size, chunk_size, cmdline, web] = process.argv.slice(2);
const m = fs.existsSync(file) ? JSON.parse(fs.readFileSync(file)) : { builds: [] };
const entry = { id, label, kernel: "Image", rootfs: { ext, size: +size, chunk_size: +chunk_size }, cmdline };
if(web) entry.web = web;
const i = m.builds.findIndex(b => b.id === id);
if(i >= 0) m.builds[i] = entry; else m.builds.push(entry);
fs.writeFileSync(file, JSON.stringify(m, null, 2) + "\n");
console.log(`${id}: ${label}, ${size} bytes in ${size / chunk_size} parts`);
JS
