#!/bin/sh
# Builds the card10 badge's firmware (https://git.flow3r.garden/card10/firmware,
# MIT unless a file says otherwise) at a pinned commit, for the
# v86-arm site's card10 (v86's Cortex-M core, src/cm/src/dev/max32665.rs):
# Epicardium (core 0, FreeRTOS) and Pycardium (core 1, MicroPython), stripped,
# and the external flash's FAT file system with the firmware's preloaded
# apps (preload/) and a card10.cfg, gzipped.
#
#   tools/a64-site/card10/build.sh [out-dir]
#
# CARD10 (default ~/repo/card10): where the firmware is checked out
# (firmware/, with its submodules). The build runs in the container image
# card10-build (Containerfile here: podman build -t card10-build
# tools/a64-site/card10), with only the checkout mounted (as /firmware).
#
# Writes epicardium.elf, pycardium.elf, fat.img.gz, api.json (api-names.py)
# and version.txt to out-dir, and debug/ for the gdb-wui image
# (tools/a64-site/gdb-wui/build.sh): the ELFs with their debug info, and the
# sources it names, under firmware/ (as /firmware, where they were built;
# listed with gdb-multiarch)
# (default build/card10), then:
#   tools/a64-site/add-card10.sh card10 "card10 badge: Epicardium and Pycardium" build/card10
set -eu
OUT=$(realpath -m "${1:-build/card10}")
CARD10=$(realpath -m "${CARD10:-$HOME/repo/card10}")
REPO=https://git.flow3r.garden/card10/firmware.git
COMMIT=71be1ddeb128e6068d2f1df605ec9c8c10f67091
SRC=$CARD10/firmware
if [ ! -d "$SRC/.git" ]; then
    mkdir -p "$CARD10"
    git clone -q "$REPO" "$SRC"
fi
git -C "$SRC" fetch -q origin "$COMMIT" 2>/dev/null || true
git -C "$SRC" checkout -q --detach "$COMMIT"
git -C "$SRC" submodule update -q --init
# The default app: the analog clock (the firmware's own default, as
# card10.cfg documents it; main.py falls back to the G-Watch face, which
# needs the BHI160 motion sensor, without one)
printf 'default_app = apps/analog_clock/__init__.py\n' > "$SRC/build-card10.cfg"
podman run --rm --userns=keep-id -v "$SRC:/firmware:z" -e HOME=/tmp card10-build sh -c '
set -e
[ -d build ] || ./bootstrap.sh >/dev/null
ninja -C build >/dev/null
arm-none-eabi-strip -o build/epicardium.elf build/epicardium/epicardium.elf
arm-none-eabi-strip -o build/pycardium.elf build/pycardium/pycardium.elf
git describe --tags --always > build/version.txt
rm -f build/fat.img
mkfs.fat -C -n CARD10 build/fat.img 8192 >/dev/null
mcopy -s -i build/fat.img preload/* ::/
mcopy -i build/fat.img build-card10.cfg ::/card10.cfg'
rm "$SRC/build-card10.cfg"
mkdir -p "$OUT"
cp "$SRC/build/epicardium.elf" "$SRC/build/pycardium.elf" "$SRC/build/version.txt" "$OUT/"
gzip -9 -c "$SRC/build/fat.img" > "$OUT/fat.img.gz"
# The API calls' and interrupts' names, for the page's view of the cores
python3 -I "$(dirname "$0")/api-names.py" "$SRC/epicardium/epicardium.h" > "$OUT/api.json"
# The debug bundle
D=$OUT/debug
rm -rf "${D:?}"
mkdir -p "$D/firmware"
cp "$SRC/build/epicardium/epicardium.elf" "$SRC/build/pycardium/pycardium.elf" "$D/"
for elf in "$D/epicardium.elf" "$D/pycardium.elf"; do
    gdb-multiarch -nx -batch "$elf" -ex 'maint expand-symtabs' -ex 'info sources' 2>/dev/null
done | tr ',' '\n' | sed 's/^ *//' | grep '^/firmware/' |
    python3 -I -c 'import os, sys; print("\n".join(sorted({os.path.relpath(os.path.normpath(l.strip()), "/firmware") for l in sys.stdin})))' |
    while read -r f; do
        if [ -f "$SRC/$f" ]; then
            mkdir -p "$D/firmware/$(dirname "$f")"
            cp "$SRC/$f" "$D/firmware/$f"
        fi
    done
echo "debug bundle: $(find "$D/firmware" -type f | wc -l) source files"
# (built in the container, under /firmware: no paths of this machine)
if grep -rl --binary-files=binary "$HOME" "$OUT/epicardium.elf" "$OUT/pycardium.elf" "$SRC/build/fat.img" "$D"; then
    echo "the firmware names this machine's paths (above)" >&2
    exit 1
fi
ls -l "$OUT"
