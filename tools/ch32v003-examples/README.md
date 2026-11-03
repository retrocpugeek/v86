# CH32V003 firmware to try

ELFs for the v86-riscv site's
[Run your own CH32V003 firmware](https://v86-riscv.turret13.workers.dev/?build=upload)
page, which lists them as examples for each board (the site serves copies
of them: tools/rv64-site/build.sh). Built by `build.sh`:

- examples of [ch32fun](https://github.com/cnlohr/ch32fun) (MIT,
  `LICENSE-ch32fun`) at a pinned commit, for the serial console board
  (`debugprintfdemo`, `uartdemo`, `systick_irq`, `template`,
  `random_numbers`, `flashtest`, `hsitrim`, `optiondata`, and `tim1_pwm`,
  which uses TIM1, not modelled yet) and the game console's OLED
  (`i2c_oled`);
- the site's other CH32V003 firmware: `tetris.elf` (tools/ch32v003),
  Maksym Curul's [Mini Game Console](https://github.com/max7d3/CH32V003-MiniGameConsole)
  (Apache-2.0, `LICENSE-minigameconsole`) and Vlad Tomoiagă's
  [osillyscope](https://github.com/tvlad1234/osillyscope) (MIT,
  `LICENSE-osillyscope`), as their build scripts build them.

They keep their symbols and line numbers, for the page's debugger (`&debug`).
