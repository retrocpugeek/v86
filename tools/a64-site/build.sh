#!/bin/sh
# Assembles the static site for hosting v86's aarch64 machine in build/a64-site/:
# the page (with guest-web.js and its service worker, sw.js), libv86.js, v86-a64.wasm and
# v86-cm.wasm (the Cortex-M core, for the Pebble watches: pebble/build.sh, add-watch.sh). Disk images are added with add-image.sh
# and kept across rebuilds.
#
#   tools/a64-site/build.sh
#   tools/a64-site/add-image.sh default "Buildroot (reference)" .../Image .../rootfs.ext4
#   FIRMWARE=1 tools/a64-site/add-image.sh u-boot "U-Boot ..." .../u-boot.bin .../disk.img  (see u-boot/build.sh)
#   npx wrangler deploy --config tools/a64-site/wrangler.jsonc
set -eu
cd "$(dirname "$0")/../.."
OUT=build/a64-site

make build/v86-a64.wasm build/v86-cm.wasm build/libv86.js
mkdir -p "$OUT/images"
cp tools/a64-site/sw.js tools/a64-site/guest-web.js tools/a64-site/_headers build/libv86.js build/v86-a64.wasm build/v86-cm.wasm "$OUT/"
# The page loads libv86.js, guest-web.js and the wasm with their content hash as a query,
# so a browser never runs a cached old one with the new page (they're cached
# for an hour)
V=$(cat build/libv86.js build/v86-a64.wasm build/v86-cm.wasm tools/a64-site/guest-web.js | sha256sum | cut -c1-12)
sed -e "s|src=\"libv86.js\"|src=\"libv86.js?v=$V\"|" -e "s|src=\"guest-web.js\"|src=\"guest-web.js?v=$V\"|" -e "s|wasm_path: \"v86-a64.wasm\"|wasm_path: \"v86-a64.wasm?v=$V\"|" -e "s|wasm_path: \"v86-cm.wasm\"|wasm_path: \"v86-cm.wasm?v=$V\"|" \
  tools/a64-site/index.html > "$OUT/index.html"
[ -f "$OUT/images/manifest.json" ] || echo '{ "builds": [] }' > "$OUT/images/manifest.json"
# Each image in a directory named after its contents (see the script)
python3 tools/version-images.py "$OUT"
# kuna, the decompiler the gdb-wui images ask this page for (see the script);
# without a kuna checkout the site has none, and their Decompiled view shows
# Ghidra's sidecars alone
if [ -d "${KUNA_SRC:-$HOME/repo/kuna}" ]; then
  tools/kuna-web/build.sh "$OUT"
else
  echo "no kuna checkout (KUNA_SRC): the site has no kuna"
fi
echo "site in $OUT ($(find "$OUT" -type f | wc -l) files)"
