#!/usr/bin/env bash
# kuna in a site's pages, for gdb-wui's decompiler (gdb-wui -kuna tcp:...,
# tools/gdb-wui-guest/S95gdb-wui): guest-web.js answers the guest's
# connections and hands the work to kuna-bridge-worker.js, which runs kuna
# (https://github.com/Noelo-Lab/kuna, Ghidra's decompiler in Rust) as
# WebAssembly with kuna's own loader, kuna-web.js.
#
#   tools/kuna-web/build.sh build/a64-site
#
# Writes <site>/kuna/v-<hash>/ (the files, in a directory named after their
# contents, as tools/version-images.py does for images) and
# <site>/kuna/manifest.json naming it. KUNA_SRC is a kuna checkout (default
# ~/repo/kuna) with its specs built (make specs); needs the Rust target
# wasm32-wasip1 (rustup target add wasm32-wasip1).
set -euo pipefail
SITE=${1:?usage: $0 <site dir>}
KUNA_SRC=${KUNA_SRC:-$HOME/repo/kuna}
HERE=$(cd "$(dirname "$0")" && pwd)
WEB=$KUNA_SRC/integrations/web
SPECS=$KUNA_SRC/specs

[ -n "$(find "$SPECS" -name '*.sla' -print -quit)" ] || { echo "no .sla in $SPECS: run make specs in $KUNA_SRC" >&2; exit 1; }
# The paths of the sources, which panic messages carry, are the builder's
# own: remapped, in a target directory of their own so as not to rebuild
# kuna's other builds every time.
TARGET_DIR=$KUNA_SRC/decompiler/target/v86-web
(cd "$KUNA_SRC/decompiler" &&
    RUSTFLAGS="--remap-path-prefix=$HOME/.cargo=/cargo --remap-path-prefix=$HOME/.rustup=/rustup --remap-path-prefix=$KUNA_SRC=/kuna" \
    cargo build --release --target wasm32-wasip1 --target-dir "$TARGET_DIR" -p kuna-wasm)
WASM=$TARGET_DIR/wasm32-wasip1/release/kuna_wasm.wasm

STAGE=$(mktemp -d)
trap 'rm -rf "${STAGE:?}"' EXIT
mkdir -p "$STAGE/vendor/browser_wasi_shim"
if command -v wasm-opt >/dev/null; then
    wasm-opt -Oz "$WASM" -o "$STAGE/kuna_wasm.wasm"
else
    cp "$WASM" "$STAGE/"
fi
cp "$WEB/kuna-web.js" "$WEB/sha256.js" "$HERE/kuna-bridge-worker.js" "$STAGE/"
cp -r "$WEB/vendor/browser_wasi_shim/dist" "$WEB/vendor/browser_wasi_shim"/LICENSE-* "$STAGE/vendor/browser_wasi_shim/"
cp "$KUNA_SRC/LICENSE" "$KUNA_SRC/NOTICE" "$STAGE/"

# The specs: every small file (the engine reads them to recognise any binary,
# as kuna's page does) and the SLEIGH tables of the machines these sites run
# (the .sla, fetched when a binary needs one).
( cd "$SPECS" && find . \( -name '*.ldefs' -o -name '*.pspec' -o -name '*.cspec' -o -name '*.dwarf' \) -print ) |
    python3 -c '
import base64, json, os, sys
root, out = sys.argv[1], {}
for rel in sorted(l.strip()[2:] for l in sys.stdin):
    with open(os.path.join(root, rel), "rb") as f:
        out[rel] = base64.b64encode(f.read()).decode()
json.dump(out, open(sys.argv[2], "w"), separators=(",", ":"))
' "$SPECS" "$STAGE/specs-small.json"
for sla in AARCH64/data/languages/AARCH64.sla RISCV/data/languages/riscv.lp64d.sla RISCV/data/languages/riscv.ilp32d.sla; do
    mkdir -p "$STAGE/specs/Ghidra/Processors/$(dirname "$sla")"
    cp "$SPECS/Ghidra/Processors/$sla" "$STAGE/specs/Ghidra/Processors/$sla"
done
# RISC-V's ELF machine is recognised as RV64GC, which only its deprecated
# languages (old/riscv_deprecated.ldefs) define, naming the current tables
# from their own directory: kuna's page fetches them from there
mkdir -p "$STAGE/specs/Ghidra/Processors/RISCV/data/languages/old"
cp "$SPECS"/Ghidra/Processors/RISCV/data/languages/riscv.*.sla "$STAGE/specs/Ghidra/Processors/RISCV/data/languages/old/"
COMMIT=$(git -C "$KUNA_SRC" rev-parse --short=12 HEAD)
printf '{"kuna":"%s","date":"%s"}\n' "$COMMIT" "$(git -C "$KUNA_SRC" log -1 --format=%cs)" > "$STAGE/version.json"

if grep -rq "$HOME" "$STAGE"; then
    echo "kuna's files name $HOME:" >&2
    grep -rl "$HOME" "$STAGE" >&2
    exit 1
fi
V=v-$(cd "$STAGE" && find . -type f -print0 | sort -z | xargs -0 sha256sum | sha256sum | cut -c1-12)
mkdir -p "$SITE/kuna"
for d in "$SITE"/kuna/v-*; do
    [ -d "$d" ] && [ "$(basename "$d")" != "$V" ] && rm -rf "${d:?}"
done
rm -rf "${SITE:?}/kuna/$V"
cp -r "$STAGE" "$SITE/kuna/$V"
chmod -R a+rX "$SITE/kuna/$V"
printf '{"dir":"%s","kuna":"%s"}\n' "$V" "$COMMIT" > "$SITE/kuna/manifest.json"
echo "kuna $COMMIT -> $SITE/kuna/$V ($(du -sh "$SITE/kuna/$V" | cut -f1))"
