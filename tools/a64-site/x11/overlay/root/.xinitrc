#!/bin/sh
# UTF-8 window titles (glibc's built-in C.UTF-8 locale)
export LANG=C.UTF-8
xsetroot -solid '#3b4252' -cursor_name left_ptr
exec openbox-session
