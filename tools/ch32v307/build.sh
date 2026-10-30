#!/bin/sh
# Builds RT-Thread (its msh shell on USART1) for v86's emulated CH32V307,
# from RT-Thread's ch32v307v-r1 BSP:
#
#   tools/ch32v307/build.sh            -> build/ch32v307/rtthread.bin, .json
#
# Needs git, scons and a bare-metal RISC-V GCC with picolibc (Ubuntu:
# gcc-riscv64-unknown-elf picolibc-riscv64-unknown-elf scons). RT-Thread (at
# RTT_COMMIT) and the BSP's CH32V307 SDK package are fetched into
# build/ch32v307/work. The BSP is copied to ch32v307-v86 and adapted:
# - the Ubuntu toolchain and picolibc instead of WCH's GCC and newlib;
# - interrupt("WCH-Interrupt-fast") (WCH's GCC: no register saving, the
#   hardware does it) becomes the standard interrupt("machine");
# - v86.c: the idle thread sleeps (wfi) instead of spinning;
# - the 256 KB flash / 64 KB SRAM split (the BSP's 224 KB is too small with
#   the demos), a thread-local storage block for picolibc (rand etc.) that
#   every thread's tp points at, the RTC (UTC), a bigger shell stack;
# - demo packages (shell commands), at pinned commits, patched (patches/):
#   snake (GPL-2.0), aclock, CoreMark, Lua 5.1 (Lua2RTT).
set -eu
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
OUT=$ROOT/build/ch32v307
WORK=${WORK:-$OUT/work}
RTT_COMMIT=8159dc81b773bd82f191696b672c69dec2742328
SDK_URL=https://github.com/kaidegit/CH32V307-SDK-for-RTT/releases/download/v2.7/CH32V307-SDK-for-RTT-v2.7.zip
SDK_SHA256=b2ea7355bf3cec7fe8b8294785d4fab187e5a51b74ed28a97e6128375a8b1711
# Demo packages: name, repository, commit
PACKAGES="snake https://github.com/mysterywolf/snake.git 7e21a23f239e2f3a388c2c63d94c69a2e7eb2691
aclock https://github.com/mysterywolf/aclock.git 7b5b2b7b6388f605416fa3f0b29a74f0d717153e
coremark https://github.com/wuhanstudio/coremark.git 80a55f92c86e038dfc7a6f7679d119084e2ee9b7
Lua2RTT https://github.com/liu2guang/Lua2RTT.git d989c309c56974cdf9f44f056c964a11ed4c84ce"
mkdir -p "$WORK"

RTT=$WORK/rt-thread
if [ ! -d "$RTT/.git" ]; then
  git init -q "$RTT"
  git -C "$RTT" remote add origin https://github.com/RT-Thread/rt-thread.git
  git -C "$RTT" sparse-checkout set src include libcpu/risc-v components tools bsp/wch/risc-v
fi
if [ "$(git -C "$RTT" rev-parse HEAD 2>/dev/null)" != "$RTT_COMMIT" ]; then
  git -C "$RTT" fetch -q --depth 1 --filter=blob:none origin "$RTT_COMMIT"
  git -C "$RTT" checkout -q -f "$RTT_COMMIT"
fi
# (the changes below edit the shared drivers and the CPU port in place)
git -C "$RTT" checkout -q -- bsp/wch/risc-v/libraries libcpu/risc-v

ZIP=$WORK/ch32v307-sdk.zip
if [ ! -f "$ZIP" ] || ! echo "$SDK_SHA256  $ZIP" | sha256sum -c --status; then
  curl -sSL -o "$ZIP" "$SDK_URL"
  echo "$SDK_SHA256  $ZIP" | sha256sum -c --status || { echo "SDK checksum mismatch" >&2; exit 1; }
fi

WCH=$RTT/bsp/wch/risc-v
BSP=$WCH/ch32v307-v86
rm -rf "$BSP" "$WORK/sdk"
cp -r "$WCH/ch32v307v-r1" "$BSP"
mkdir -p "$WORK/sdk" "$BSP/packages"
(cd "$WORK/sdk" && unzip -q "$ZIP")
mv "$WORK/sdk/CH32V307-SDK-for-RTT-main" "$BSP/packages/ch32v307_sdk-v2.7"
cat > "$BSP/packages/SConscript" <<'PY'
import os
from building import *
objs = []
cwd = GetCurrentDir()
for item in os.listdir(cwd):
    if os.path.isfile(os.path.join(cwd, item, 'SConscript')):
        objs = objs + SConscript(os.path.join(item, 'SConscript'))
Return('objs')
PY
sed -i -e "s|^    EXEC_PATH   = .*|    EXEC_PATH   = '/usr/bin'|" \
  -e "s|PREFIX = 'riscv-none-embed-'|PREFIX = 'riscv64-unknown-elf-'|" \
  -e "s|DEVICE = ' -march=rv32imac -mabi=ilp32 |DEVICE = ' -march=rv32imac_zicsr_zifencei -mabi=ilp32 --specs=picolibc.specs |" \
  -e "/CFLAGS += ' -save-temps=obj'/d" "$BSP/rtconfig.py"
grep -rl 'WCH-Interrupt-fast' "$BSP" "$WCH/libraries" | xargs sed -i 's/interrupt("WCH-Interrupt-fast")/interrupt("machine")/'
cp "$HERE/v86.c" "$BSP/applications/"
sed -i "s/src = \['main.c'\]/src = ['main.c', 'v86.c']/" "$BSP/applications/SConscript"

# picolibc: RT-Thread's stdio glue checks TINY_STDIO (newer picolibc says
# __TINY_STDIO); Lua's rodata check wants _stext
sed -i -e "s|--specs=picolibc.specs |--specs=picolibc.specs -DTINY_STDIO |" \
  -e "s|^    LFLAGS += ' -T ' + LINK_FILE$|    LFLAGS += ' -T ' + LINK_FILE + ' -Wl,--defsym=_stext=_sinit'|" "$BSP/rtconfig.py"
# 256 KB flash / 64 KB SRAM, and the TLS block (in .data, so it's copied
# from flash at reset)
LDS=$BSP/board/linker_scripts/link.lds
sed -i -e 's/LENGTH = 224K/LENGTH = 256K/' -e 's/LENGTH = 96K/LENGTH = 64K/' \
  -e 's|^        \*(.srodata .srodata.\*)$|&\n        . = ALIGN(8);\n        PROVIDE( __tls_base = . );\n        *(.tdata .tdata.* .gnu.linkonce.td.*)\n        *(.tbss .tbss.* .gnu.linkonce.tb.*)|' "$LDS"
grep -q __tls_base "$LDS"
sed -i 's/#define SRAM_SIZE  96/#define SRAM_SIZE  64/' "$BSP/board/board.h"
sed -i 's|^    frame->epc     = (rt_ubase_t)tentry;$|&\n    { extern char __tls_base[]; frame->tp = (rt_ubase_t)__tls_base; } /* v86: picolibc TLS */|' \
  "$RTT/libcpu/risc-v/legacy_common/cpuport.c"
grep -q __tls_base "$RTT/libcpu/risc-v/legacy_common/cpuport.c"

# Configuration
CFG=$BSP/rtconfig.h
sed -i -e 's/#define RT_NAME_MAX 12/#define RT_NAME_MAX 16/' \
  -e 's/#define RT_SERIAL_RB_BUFSZ [0-9]*/#define RT_SERIAL_RB_BUFSZ 512/' \
  -e 's/#define FINSH_THREAD_STACK_SIZE 2048/#define FINSH_THREAD_STACK_SIZE 8192/' \
  -e 's/#define RT_LIBC_TZ_DEFAULT_HOUR 8/#define RT_LIBC_TZ_DEFAULT_HOUR 0/' "$CFG"
python3 - "$CFG" <<'PY'
import sys
p = sys.argv[1]
s = open(p).read()
i = s.rindex("#endif")
s = s[:i] + """/* v86: the RTC (time for aclock and date) */
#define RT_USING_RTC
#define BSP_USING_RTC
#define BSP_USING_RTC_LSE

/* v86: demo packages */
#define PKG_USING_SNAKE
#define PKG_USING_ACLOCK
#define PKG_USING_COREMARK
#define COREMARK_ITERATIONS 3600
#define CORE_MARK_HAS_FLOAT 0
#define PKG_USING_LUA
#define LUA_USING_PORTING_V514

""" + s[i:]
open(p, "w").write(s)
PY

# Demo packages
echo "$PACKAGES" | while read -r NAME URL COMMIT; do
  SRC=$WORK/pkg-$NAME
  if [ "$(git -C "$SRC" rev-parse HEAD 2>/dev/null)" != "$COMMIT" ]; then
    rm -rf "$SRC"
    git init -q "$SRC"
    git -C "$SRC" fetch -q --depth 1 "$URL" "$COMMIT"
    git -C "$SRC" checkout -q FETCH_HEAD
  fi
  mkdir -p "$BSP/packages/$NAME"
  git -C "$SRC" archive HEAD | tar -x -C "$BSP/packages/$NAME"
  if [ -f "$HERE/patches/$NAME.patch" ]; then
    patch -s -d "$BSP/packages/$NAME" -p1 < "$HERE/patches/$NAME.patch"
  fi
done

(cd "$BSP" && scons -s -j"$(nproc)")
cp "$BSP/rtthread.bin" "$BSP/rtthread.elf" "$OUT/"
set -- $(riscv64-unknown-elf-size "$OUT/rtthread.elf" | tail -1)
printf '{ "flash": %d, "flash_size": 262144, "text": %d, "data": %d, "bss": %d, "ram_size": 65536 }\n' \
  "$(stat -c %s "$OUT/rtthread.bin")" "$1" "$2" "$3" > "$OUT/rtthread.json"
cat "$OUT/rtthread.json"
