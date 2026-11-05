# Contiki's desktop on the card10 (see main.c): built in the firmware's
# tree as l0dables/ctk, with Contiki's sources in contiki/
# (tools/a64-site/card10/build.sh puts them there).
name = 'ctk'

ctk_contiki = [
  'contiki/core/sys/process.c',
  'contiki/core/sys/etimer.c',
  'contiki/core/sys/timer.c',
  'contiki/core/ctk/ctk.c',
  'contiki/core/ctk/ctk-conio.c',
  'contiki/apps/calc/calc.c',
  'contiki/apps/about/about.c',
  'contiki/apps/process-list/process-list.c',
]

elf = executable(
  name + '.elf',
  'main.c',
  'conio-card10.c',
  'serial.c',
  ctk_contiki,
  build_by_default: true,
  include_directories: include_directories('.', 'contiki/core', 'contiki/core/ctk'),
  # (Contiki's code: its own style)
  c_args: ['-Wno-sign-compare', '-Wno-unused-variable', '-Wno-unused-but-set-variable',
           '-Wno-implicit-fallthrough', '-Wno-pointer-sign'],
  override_options: ['c_std=gnu99'],
  dependencies: [l0dable_startup, api_caller],
  link_whole: [l0dable_startup_lib],
  link_args: [
    '-Wl,-Map=' + meson.current_build_dir() + '/' + name + '.map',
  ],
  pie: true,
)
