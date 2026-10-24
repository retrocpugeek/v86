#!/bin/sh
# Debugs CH32V003 firmware through rv64run's GDB stub (--gdb, src/rv64/src/gdb.rs)
# with the host's gdb-multiarch, as a debug probe on the board would be used:
# the console Tetris (tools/ch32v003) and the game console's Tiny Tris
# (tools/ch32v003-gameconsole), from their ELFs, checking what gdb says.
#
#   tests/rv64/gdb-stub.sh
#   STUB=node tests/rv64/gdb-stub.sh
#
# STUB=node: the stub in the browser build instead, through the V86 API, as
# the site's pages use it (tests/rv64/gdb-bridge.mjs; make build/v86-rv64.wasm).
#
# Needs gdb-multiarch, rv64run (cd src/rv64 && cargo build --release --bin
# rv64run) and the firmware (tools/ch32v003/build.sh,
# tools/ch32v003-gameconsole/build.sh).
set -eu
cd "$(dirname "$0")/../.."
RUN=src/rv64/target/release/rv64run
TMP=$(mktemp -d)
PORT=${PORT:-3333}
trap 'kill "$(cat "$TMP/pid" 2>/dev/null)" 2>/dev/null || true; rm -rf "${TMP:?}"' EXIT
fail=0

# session <name> <firmware.bin> <firmware.elf> <expected...>: runs gdb's
# commands from stdin against a fresh machine held at reset, and checks that
# each expected line (a grep -E pattern) is in what gdb printed.
session() {
    name=$1 bin=$2 elf=$3
    shift 3
    if [ "${STUB:-}" = node ]; then
        node tests/rv64/gdb-bridge.mjs ch32v003 "$bin" "$PORT" >/dev/null 2>"$TMP/stub.err" &
        echo $! > "$TMP/pid"
        sleep 1.5
    else
        "$RUN" --ch32v003 "$bin" --gdb "127.0.0.1:$PORT" --gdb-wait >/dev/null 2>"$TMP/stub.err" &
        echo $! > "$TMP/pid"
        sleep 0.3
    fi
    { printf 'set pagination off\nset confirm off\nfile %s\ntarget remote 127.0.0.1:%s\n' "$elf" "$PORT"; cat; echo detach; } > "$TMP/cmds"
    timeout 60 gdb-multiarch -nx -batch -x "$TMP/cmds" > "$TMP/out" 2>&1 || true
    kill "$(cat "$TMP/pid")" 2>/dev/null || true
    wait "$(cat "$TMP/pid")" 2>/dev/null || true
    bad=0
    for want in "$@"; do
        if ! grep -qE -- "$want" "$TMP/out"; then
            echo "FAIL $name: no line matching: $want"
            bad=1
        fi
    done
    if [ $bad = 0 ]; then echo "ok   $name"; else fail=1; sed 's/^/     | /' "$TMP/out"; fi
}

session tetris build/ch32v003/tetris.bin build/ch32v003/tetris.elf \
    '^pc +0x0[[:space:]]' \
    'currently "riscv:rv32"' \
    'Breakpoint 1, main \(\) at .*tetris\.c' \
    'Breakpoint 2, draw \(\) at .*tetris\.c' \
    '^#1 .* in main \(\) at .*tetris\.c' \
    '^a5[[:space:]]' \
    '^\$1 = 4242$' \
    '^\$2 = 0x904$' \
    'Reset, stopped at the reset vector' \
    '^\$3 = .* 0x0 <_start>' <<'EOF'
info registers pc
show architecture
break main
continue
break draw
continue
bt
info registers
set var score = 4242
p score
p/x $mtvec
monitor reset halt
maint flush register-cache
p $pc
continue
EOF
# RV32E: no a6
if grep -qE '^a6[[:space:]]' "$TMP/out"; then echo "FAIL tetris: registers RV32E doesn't have"; fail=1; fi
# After the reset, main again: its breakpoint is kept
if [ "$(grep -c '^Breakpoint 1, main' "$TMP/out")" != 2 ]; then echo "FAIL tetris: main's breakpoint not hit after monitor reset"; fail=1; fi

G=build/ch32v003-gameconsole/src/software/tiny_tris/bin/tiny_tris.elf
session tiny_tris build/ch32v003-gameconsole/tiny_tris.bin "$G" \
    'Breakpoint 1, I2C_write \(data=' \
    'in OLED_init \(\) at src/oled_min\.c' \
    'Breakpoint 2, ADC_read \(\) at src/gpio\.h' \
    'Value returned is \$1 = ' <<'EOF'
break I2C_write
continue
bt
delete
break ADC_read
continue
finish
EOF
exit $fail
