# Tiny BASIC for the CH32V003

A bare-metal Tiny BASIC, with a Wozmon-style machine monitor, for the WCH
CH32V003 (RISC-V RV32EC, 16 KB of flash, 2 KB of RAM), over USART1 at
115200 baud. It's 6 KB of plain C, built like
[Tetris](../ch32v003) with its startup code and linker script, and runs
in your browser on the v86-riscv site's emulated CH32V003:

- [Run it](https://v86-riscv.turret13.workers.dev/?build=ch32v003-basic)
- [Debug it with gdb-wui](https://v86-riscv.turret13.workers.dev/?build=ch32v003-basic&debug)

```
TINY BASIC FOR THE CH32V003
888 BYTES FREE
> 10 FOR I=1 TO 5: PRINT I*I;" ";: NEXT
> RUN
1 4 9 16 25
```

## Example programs

In [examples](examples), and on the site as buttons under the terminal,
each typing its program in and running it:

| | |
| --- | --- |
| [counter.bas](examples/counter.bas) | counts to 15 in binary on the LEDs, with `POKE` |
| [scanner.bas](examples/scanner.bas) | a light sweeping across the LEDs, until Ctrl-C |
| [primes.bas](examples/primes.bas) | the primes below 300 |
| [guess.bas](examples/guess.bas) | a number guessing game, with `INPUT` and `RND` |
| [fizzbuzz.bas](examples/fizzbuzz.bas) | FizzBuzz to 30 |
| [speed.bas](examples/speed.bas) | how many loops a second this BASIC runs (about 6,500) |
| [mandelbrot.bas](examples/mandelbrot.bas) | the Mandelbrot set in integers, in 13 seconds |

To type one in yourself, paste it into the terminal while nothing is
running.

## The language

Integers are 32-bit, and the variables are `A` to `Z`. A line with a
number goes into the program (a number alone deletes that line); a line
without one runs at once. Statements on a line are separated by `:`.

| Statement | |
| --- | --- |
| `PRINT` or `?` | items separated by `;` (nothing between) or `,` (a space): `"text"`, expressions, `HEX(x)` (8 hex digits); a trailing `;` or `,` keeps the line going |
| `LET v = x` | `LET` is optional |
| `IF x THEN statement` | or `THEN line`; when `x` is 0 the rest of the line is skipped |
| `GOTO line`, `GOSUB line`, `RETURN` | 8 `GOSUB`s deep |
| `FOR v = a TO b [STEP c]`, `NEXT [v]` | 6 loops deep |
| `INPUT ["prompt";] v[, v]` | each value is an expression |
| `POKE address, value` | a 32-bit store, e.g. to a peripheral register |
| `DELAY ms` | |
| `REM` or `'`, `END`, `STOP`, `CLS` | |
| `RUN`, `LIST`, `NEW` | |
| `SAVE`, `LOAD` | the program into, and back from, the last 1 KB of flash |
| `MON` | the monitor |

Expressions have `+ - * /` and `MOD`, comparisons `= <> < > <= >=` (true
is -1), and bitwise `AND`, `OR`, `XOR` and `NOT`, which bind least tightly in
that order: `OR` and `XOR`, then `AND`, then comparisons. Numbers are
decimal or `$` hex. Functions: `PEEK(address)` (a 32-bit load), `RND(n)` (1
to n), `ABS(x)`, `TIME` (ms since reset) and `FREE` (program bytes left).

Ctrl-C stops a running program. An address that isn't there, read or
written, is reported as a fault (`?LOAD ACCESS FAULT AT ...`) instead of
hanging the chip. `INPUT` asks again (`?REDO`) for a reply that isn't an
expression.

### The LEDs: PC0 to PC3

The site shows PC0 to PC3 as LEDs. This counts to 15 in binary on them:

```
10 REM COUNT IN BINARY ON PC0-PC3
20 POKE $40021018, PEEK($40021018) OR $10
30 POKE $40011000, PEEK($40011000) AND NOT $FFFF OR $1111
40 FOR I=0 TO 15
50 POKE $4001100C, I
60 DELAY 100
70 NEXT I
```

Line 20 turns on GPIO port C's clock (RCC's APB2PCENR), line 30 makes PC0
to PC3 push-pull outputs (GPIOC's CFGLR), and line 50 sets them (OUTDR).

## The monitor

`MON` starts it; the prompt is `*`, and everything is in hex, a 32-bit
word at a time, as in the Apple-1's Wozmon:

| | |
| --- | --- |
| `20000000` | shows the word there |
| `20000000.2000001F` | shows the words from one address to the other |
| `20000000: 1 2 3` | stores words from the address on |
| `address R` | calls the code there |
| `Q` or Ctrl-C | back to BASIC |

## Memory

Of the 2 KB of RAM, 888 bytes hold the program and about 700 are the
stack. That's enough for expressions 8 levels of parentheses deep
(`?TOO COMPLEX` beyond that). Lines are kept as typed, in capitals.

## Building

```
tools/ch32v003-basic/build.sh [out-dir]
```

This needs a RISC-V GCC that supports rv32ec/ilp32e (e.g. Ubuntu's
gcc-riscv64-unknown-elf). It writes `basic.elf`, `basic.bin` and
`basic.json` (sizes, for the site) to `build/ch32v003-basic`.
`tools/rv64-site/add-mcu.sh` shows how the site adds it.
