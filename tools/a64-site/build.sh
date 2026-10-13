#!/bin/sh
# Assembles the static site for hosting v86's aarch64 machine in build/a64-site/:
# the page, libv86.js and v86-a64.wasm. Disk images are added with add-image.sh
# and kept across rebuilds.
#
#   tools/a64-site/build.sh
#   tools/a64-site/add-image.sh default "Buildroot (reference)" .../Image .../rootfs.ext4
#   npx wrangler deploy --config tools/a64-site/wrangler.jsonc
set -eu
cd "$(dirname "$0")/../.."
OUT=build/a64-site

make build/v86-a64.wasm build/libv86.js
mkdir -p "$OUT/images"
cp tools/a64-site/index.html tools/a64-site/_headers build/libv86.js build/v86-a64.wasm "$OUT/"
[ -f "$OUT/images/manifest.json" ] || echo '{ "builds": [] }' > "$OUT/images/manifest.json"
echo "site in $OUT ($(find "$OUT" -type f | wc -l) files)"
