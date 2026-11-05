# Contiki's desktop on the card10

[Contiki](https://github.com/contiki-os/contiki)'s desktop, CTK, as a native
app (an l0dable) on the card10 badge's core 1, on Epicardium's API, with a
RetroCPUGeek theme: the RetroCPUGeek avatar's chip on a circuit board as the
backdrop, an RCG menu, and a window naming the chip it really runs on, the
card10's MAX32666.

![The desktop, with the calculator (card10run, 4x)](desktop.png)

On the v86-arm site: `?build=card10-ctk` starts it; on `?build=card10` it's
in the badge's menu as "Contiki desktop (RCG)".

- `main.c`: Contiki's platform (clock, main loop), keys from the buttons, the
  touch panel as CTK's mouse, and the desktop: icons and the RCG menu for
  Contiki's calculator, process list and about box, and the RetroCPUGeek
  window.
- `conio-card10.c`: the character screen CTK draws on (through its
  `ctk-conio.c`): 40 x 13 cells of 4 x 6 pixels on the 160 x 80 display,
  with line characters for the window frames, icon tiles and the backdrop.
- `contiki-conf.h`: Contiki's configuration (keys, colours, sizes).
- `contiki-ctk-conio.patch`: icons and the desktop in their own colours.
- `gen-assets.py` makes `assets.h`: the X11 misc-fixed 4x6 font (public
  domain), the backdrop (from `retrocpugeek.png`) and the icons.
- `meson.build`, `metadata.json`: built in the firmware's tree as
  `l0dables/ctk`, by `../build.sh`, which also puts it on the badge's file
  system (`/apps/ctk`, with `execute_elf = true` in `card10.cfg`).

Buttons: bottom left and right move between widgets (or menu items), top
right selects; held, top right opens and closes the menus, bottom right
goes to the next window (in the menus, the next menu) and bottom left closes
the window in front.

Touch: the emulator gives its card10 a touch panel (an FT6x36 on the
badge's I2C bus, `src/cm/src/dev/max32665.rs`, 640 x 320 points), which
Epicardium reads with `epic_touch_read()` (`../epicardium-touch.patch`) and
the page feeds from the pointer on the badge's screen. Tap a widget, an icon
or a menu; tap a window's title, then where it should go.

Debugging: `?build=card10-ctk&debug` (or `?build=card10&debug`, then start
the desktop from the badge's menu). The gdb-wui image has `ctk.elf` with its
debug info and sources (`../build.sh`'s debug bundle), and core 1's gdb loads
its symbols where Epicardium's loader always puts it, 0x20040000
(`add-symbol-file ctk.elf -o 0x20040000`, `tools/gdb-wui-guest/S95gdb-wui`),
next to Pycardium's (in flash, so the pc tells them apart). Break on
`poll_buttons`, `conio_flush`, `calc_process`'s thread function and so on.
Contiki's processes are protothreads: their locals don't live across a wait,
so watch their `static`s instead.

Tests: `tests/cm/card10-ctk.mjs` (the page);
`BUILD=card10-ctk CALL=epic_buttons_read CALLER=poll_buttons tests/guest-web/card10-debug.mjs`
(the debuggers); natively,
`card10run --keys 'rt@19:0.8,...' --touch 'X,Y@S,...'`.

Contiki is BSD-3-Clause (its `LICENSE`, copied into the build); the
RetroCPUGeek avatar is its owner's.
