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
# With additions of ours: Contiki's desktop, CTK, an l0dable in the
# badge's apps (ctk/), a touch panel driver for Epicardium
# (epicardium-touch.patch), for the emulator's card10, and an animation
# decoder in Epicardium for the desktop's player (epicardium-anim.patch).
#
# Writes epicardium.elf, pycardium.elf, fat.img.gz (and fat-ctk.img.gz,
# which starts the desktop), api.json (api-names.py)
# and version.txt to out-dir, and debug/ for the gdb-wui image
# (tools/a64-site/gdb-wui/build.sh): the ELFs (Contiki's desktop's too) with their debug info, and the
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
# (and ELF apps allowed, for Contiki's desktop below)
printf 'default_app = apps/analog_clock/__init__.py\nexecute_elf = true\n' > "$SRC/build-card10.cfg"
# and a second file system that starts the desktop instead: fat-ctk.img.gz
printf 'default_app = /apps/ctk/ctk.elf\nexecute_elf = true\n' > "$SRC/build-card10-ctk.cfg"
# Contiki's desktop, CTK, as an l0dable on core 1 (ctk/; its main.c): built
# in the firmware's tree as l0dables/ctk, with the parts of Contiki
# (https://github.com/contiki-os/contiki, BSD-3-Clause) it uses, at a pinned
# commit, in CONTIKI (default ~/repo/contiki)
CONTIKI=$(realpath -m "${CONTIKI:-$HOME/repo/contiki}")
CONTIKI_COMMIT=32b5b17f674232867c22916bb2e2534c8e9a92ff
[ -d "$CONTIKI/.git" ] || git clone -q https://github.com/contiki-os/contiki.git "$CONTIKI"
git -C "$CONTIKI" fetch -q origin "$CONTIKI_COMMIT" 2>/dev/null || true
CTK=$SRC/l0dables/ctk
# (the firmware's tree as it was, afterwards)
trap 'git -C "$SRC" checkout -q -- l0dables/meson.build epicardium/epicardium.h epicardium/drivers/meson.build \
          epicardium/modules/meson.build epicardium/modules/hardware.c epicardium/modules/modules.h
      rm -rf "${CTK:?}" "$SRC/epicardium/drivers/touch.c" "$SRC/epicardium/modules/anim.c" "$SRC/build-card10-ctk.cfg"' EXIT
# Epicardium gets a touch panel driver, epic_touch_read() (the emulator's
# card10 has one, dev/max32665.rs Ft6x36), and an animation decoder that
# runs on core 0 for core 1 (epic_anim_*, EPIC_INT_ANIM_FRAME), which the
# desktop uses
patch -s -p1 -d "$SRC" < "$(dirname "$0")/epicardium-touch.patch"
patch -s -p1 -d "$SRC" < "$(dirname "$0")/epicardium-anim.patch"
rm -rf "${CTK:?}"
mkdir -p "$CTK/contiki/core" "$CTK/contiki/apps"
cp "$(dirname "$0")"/ctk/*.c "$(dirname "$0")"/ctk/*.h "$(dirname "$0")"/ctk/meson.build "$(dirname "$0")"/ctk/metadata.json "$CTK/"
git -C "$CONTIKI" archive "$CONTIKI_COMMIT" LICENSE core/contiki.h core/contiki-version.h \
    core/contiki-default-conf.h core/sys core/lib core/ctk apps/calc apps/about apps/process-list |
    tar -x -C "$CTK/contiki"
patch -s -p1 -d "$CTK/contiki" < "$(dirname "$0")/ctk/contiki-ctk-conio.patch"
# the player's animations (ctk/gen-anims.py; Pillow)
python3 -I "$(dirname "$0")/ctk/gen-anims.py" "$CTK/anims" >/dev/null
grep -q "subdir('ctk/')" "$SRC/l0dables/meson.build" || printf "subdir('ctk/')\n" >> "$SRC/l0dables/meson.build"
podman run --rm --userns=keep-id -v "$SRC:/firmware:z" -e HOME=/tmp card10-build sh -c '
set -e
[ -d build ] || ./bootstrap.sh >/dev/null
ninja -C build > build/ninja.log 2>&1 || { tail -40 build/ninja.log; exit 1; }
arm-none-eabi-strip -o build/epicardium.elf build/epicardium/epicardium.elf
arm-none-eabi-strip -o build/pycardium.elf build/pycardium/pycardium.elf
git describe --tags --always > build/version.txt
rm -f build/fat.img
mkfs.fat -C -n CARD10 build/fat.img 8192 >/dev/null
mcopy -s -i build/fat.img preload/* ::/
mcopy -i build/fat.img build-card10.cfg ::/card10.cfg
mmd -i build/fat.img ::/apps/ctk
mcopy -i build/fat.img l0dables/ctk/metadata.json ::/apps/ctk/metadata.json
mcopy -i build/fat.img build/l0dables/ctk/ctk.elf ::/apps/ctk/ctk.elf
mcopy -i build/fat.img l0dables/ctk/contiki/LICENSE ::/apps/ctk/LICENSE.contiki
mmd -i build/fat.img ::/apps/ctk/anims
mcopy -i build/fat.img l0dables/ctk/anims/*.rla ::/apps/ctk/anims/
cp build/fat.img build/fat-ctk.img
mcopy -o -i build/fat-ctk.img build-card10-ctk.cfg ::/card10.cfg'
rm "$SRC/build-card10.cfg"
mkdir -p "$OUT"
cp "$SRC/build/epicardium.elf" "$SRC/build/pycardium.elf" "$SRC/build/version.txt" "$OUT/"
gzip -9 -c "$SRC/build/fat.img" > "$OUT/fat.img.gz"
gzip -9 -c "$SRC/build/fat-ctk.img" > "$OUT/fat-ctk.img.gz"
# The API calls' and interrupts' names, for the page's view of the cores
python3 -I "$(dirname "$0")/api-names.py" "$SRC/epicardium/epicardium.h" > "$OUT/api.json"
# The debug bundle
D=$OUT/debug
rm -rf "${D:?}"
mkdir -p "$D/firmware"
cp "$SRC/build/epicardium/epicardium.elf" "$SRC/build/pycardium/pycardium.elf" "$D/"
# (and Contiki's desktop, which core 1's gdb loads at the address the
# loader puts it, 0x20040000: S95gdb-wui)
cp "$SRC/build/l0dables/ctk/ctk.elf" "$D/"
for elf in "$D/epicardium.elf" "$D/pycardium.elf" "$D/ctk.elf"; do
    gdb-multiarch -nx -batch "$elf" -ex 'maint expand-symtabs' -ex 'info sources' 2>/dev/null
done | tr ',' '\n' | sed 's/^ *//' | grep '^/firmware/' |
    python3 -I -c 'import os, sys; print("\n".join(sorted({os.path.relpath(os.path.normpath(l.strip()), "/firmware") for l in sys.stdin})))' |
    while read -r f; do
        if [ -f "$SRC/$f" ]; then
            mkdir -p "$D/firmware/$(dirname "$f")"
            cp "$SRC/$f" "$D/firmware/$f"
        fi
    done
cp "$CTK/contiki/LICENSE" "$D/firmware/l0dables/ctk/contiki/LICENSE"
echo "debug bundle: $(find "$D/firmware" -type f | wc -l) source files"
# (built in the container, under /firmware: no paths of this machine)
if grep -rl --binary-files=binary "$HOME" "$OUT/epicardium.elf" "$OUT/pycardium.elf" "$SRC/build/fat.img" "$SRC/build/fat-ctk.img" "$D"; then
    echo "the firmware names this machine's paths (above)" >&2
    exit 1
fi
ls -l "$OUT"
