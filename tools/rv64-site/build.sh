#!/bin/sh
# Assembles the static site for hosting v86's RISC-V machine in build/rv64-site/:
# the page, libv86.js and v86-rv64.wasm, plus the service worker and the guest
# web interface code shared with the arm site (tools/a64-site/sw.js and
# guest-web.js). Disk images are added with add-image.sh
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
cp tools/rv64-site/_headers tools/a64-site/sw.js tools/a64-site/guest-web.js build/libv86.js build/v86-rv64.wasm "$OUT/"
# The page loads libv86.js and the wasm with their content hash as a query,
# so a browser never runs a cached old one with the new page (they're cached
# for an hour)
V=$(cat build/libv86.js build/v86-rv64.wasm tools/a64-site/guest-web.js | sha256sum | cut -c1-12)
sed -e "s|src=\"libv86.js\"|src=\"libv86.js?v=$V\"|" -e "s|src=\"guest-web.js\"|src=\"guest-web.js?v=$V\"|" -e "s|wasm_path: \"v86-rv64.wasm\"|wasm_path: \"v86-rv64.wasm?v=$V\"|" \
  tools/rv64-site/index.html > "$OUT/index.html"
[ -f "$OUT/images/manifest.json" ] || echo '{ "builds": [] }' > "$OUT/images/manifest.json"
# Each image in a directory named after its contents (see the script)
python3 tools/version-images.py "$OUT"
echo "site in $OUT ($(find "$OUT" -type f | wc -l) files)"
