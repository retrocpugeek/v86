#!/bin/sh
# Assembles the static site for hosting v86's RISC-V machine in build/rv64-site/:
# the page, libv86.js and v86-rv64.wasm, plus the service worker shared with
# the arm site (tools/a64-site/sw.js). Disk images are added with add-image.sh
# and kept across rebuilds.
#
#   tools/rv64-site/build.sh
#   tools/rv64-site/add-image.sh default "Buildroot" .../fw_dynamic.bin .../Image .../rootfs.squashfs
#   npx wrangler deploy --config tools/rv64-site/wrangler.jsonc
set -eu
cd "$(dirname "$0")/../.."
OUT=build/rv64-site

make build/v86-rv64.wasm build/libv86.js
mkdir -p "$OUT/images"
cp tools/rv64-site/index.html tools/rv64-site/_headers tools/a64-site/sw.js build/libv86.js build/v86-rv64.wasm "$OUT/"
[ -f "$OUT/images/manifest.json" ] || echo '{ "builds": [] }' > "$OUT/images/manifest.json"
echo "site in $OUT ($(find "$OUT" -type f | wc -l) files)"
