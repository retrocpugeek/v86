#!/bin/sh
# Adds (or replaces) firmware + kernel + rootfs in build/rv64-site/images/<id>/
# and lists it in images/manifest.json. The rootfs is split into 1 MB parts,
# which the page loads on demand (Cloudflare's static assets allow at most
# 25 MiB per file).
#
#   tools/rv64-site/add-image.sh <id> <label> <fw_dynamic.bin> <Image> <rootfs> [cmdline]
#
# The kernel is stored gzipped (Image.gz). <fw_dynamic.bin> is OpenSBI built for the generic platform (Buildroot's
# qemu_riscv64_virt_defconfig builds it). WEB=<path> shows the guest's web
# interface at that path next to the console; MEMORY=<MB> sets its RAM
# (default 256); ARCH=riscv32 marks an RV32 image (qemu_riscv32_virt_defconfig).
set -eu
[ $# -ge 5 ] || { echo "usage: $0 <id> <label> <fw_dynamic.bin> <Image> <rootfs> [cmdline]" >&2; exit 1; }
ID=$1 LABEL=$2 BIOS=$3 KERNEL=$4 ROOTFS=$5
CMDLINE=${6:-"rootwait root=/dev/vda ro console=ttyS0"}
case "$ID" in *[!a-z0-9._-]*|"") echo "bad id: $ID" >&2; exit 1;; esac
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
DIR=$ROOT/build/rv64-site/images/$ID
CHUNK=1048576
EXT=.${ROOTFS##*.}

rm -rf "$DIR"
mkdir -p "$DIR/rootfs"
cp -L "$BIOS" "$DIR/fw_dynamic.bin"
# Gzipped: Image is over the 25 MiB limit; the page decompresses it
gzip -9 -n -c "$KERNEL" > "$DIR/Image.gz"
python3 "$ROOT/tools/split-image.py" $CHUNK "$ROOTFS" "$DIR/rootfs/%d-%d$EXT"
# split-image.py pads the last part; the disk size is rounded up to match
SIZE=$(( ( $(stat -Lc %s "$ROOTFS") + CHUNK - 1 ) / CHUNK * CHUNK ))

node - "$ROOT/build/rv64-site/images/manifest.json" "$ID" "$LABEL" "$EXT" "$SIZE" "$CHUNK" "$CMDLINE" "${WEB:-}" "${MEMORY:-}" "${ARCH:-}" <<'JS'
const fs = require("fs");
const [file, id, label, ext, size, chunk_size, cmdline, web, memory, arch] = process.argv.slice(2);
const m = fs.existsSync(file) ? JSON.parse(fs.readFileSync(file)) : { builds: [] };
const entry = { id, label, bios: "fw_dynamic.bin", kernel: "Image.gz", rootfs: { ext, size: +size, chunk_size: +chunk_size }, cmdline };
if(web) entry.web = web;
if(memory) entry.memory_mb = +memory;
if(arch) entry.arch = arch;
const i = m.builds.findIndex(b => b.id === id);
if(i >= 0) m.builds[i] = entry; else m.builds.push(entry);
fs.writeFileSync(file, JSON.stringify(m, null, 2) + "\n");
console.log(`${id}: ${label}, ${size} bytes in ${size / chunk_size} parts`);
JS
