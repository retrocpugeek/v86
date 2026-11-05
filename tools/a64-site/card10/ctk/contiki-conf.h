/*
 * Contiki's configuration for the card10 (CTK, its desktop, on core 1 as an
 * l0dable; main.c): a 40 x 13 character screen in 4 x 6 pixel cells on the
 * 160 x 80 display (conio-card10.c), driven by the badge's buttons and,
 * where there is one, a touch panel (the "mouse").
 */
#ifndef CONTIKI_CONF_H_
#define CONTIKI_CONF_H_

#include <stdint.h>
#include <string.h>

#define CC_CONF_REGISTER_ARGS          1
#define CC_CONF_FUNCTION_POINTER_ARGS  1
#define CC_CONF_VA_ARGS                1
/* (symbols exported to loadable programs: not used here) */
#define CCIF
#define CLIF

typedef uint32_t clock_time_t;
#define CLOCK_CONF_SECOND 1000

/* (no networking, no energy accounting) */
typedef unsigned short uip_stats_t;
#define ENERGEST_CONF_ON 0

/* The desktop */
#define CTK_CONF_WINDOWS        1
#define CTK_CONF_MENUS          1
#define CTK_CONF_ICONS          1
#define CTK_CONF_ICON_TEXTMAPS  1
#define CTK_CONF_ICON_BITMAPS   0
#define CTK_CONF_WINDOWMOVE     1
#define CTK_CONF_WINDOWCLOSE    1
#define CTK_CONF_WIDGET_FLAGS   1
#define CTK_CONF_SCREENSAVER    0
#define CTK_CONF_MOUSE_SUPPORT  1
#define CTK_CONF_MENUWIDTH      15

/* Keys (main.c makes them from the buttons) */
#define CH_ENTER       '\n'
#define CH_DEL         '\b'
#define CH_CURS_UP     0x11
#define CH_CURS_DOWN   0x12
#define CH_CURS_LEFT   0x13
#define CH_CURS_RIGHT  0x14
#define CH_F1          0x15
#define CH_F3          0x16
#define CH_PREV_WIDGET 0x17
#define CH_NEXT_WIDGET 0x18
#define CTK_CONF_MENU_KEY          CH_F1
#define CTK_CONF_WINDOWSWITCH_KEY  CH_F3
#define CTK_CONF_WIDGETUP_KEY      CH_PREV_WIDGET
#define CTK_CONF_WIDGETDOWN_KEY    CH_NEXT_WIDGET
#define ctk_arch_isprint(c)  ((unsigned char)(c) >= ' ' && (unsigned char)(c) < 0x7f)
unsigned char ctk_arch_keyavail(void);
char ctk_arch_getkey(void);

/* Characters the screen draws as lines (conio-card10.c) */
#define CH_ULCORNER  0x01
#define CH_URCORNER  0x02
#define CH_LLCORNER  0x03
#define CH_LRCORNER  0x04
#define CH_HLINE     0x05
#define CH_VLINE     0x06
/* A cell of the video picture (conio_video) */
#define CH_VIDEO     0x07

/* Colours: indexes into conio-card10.c's palette */
#define SCREENCOLOR          0
#define BORDERCOLOR          0
#define BACKGROUNDCOLOR      0
#define WINDOWCOLOR          1
#define WINDOWCOLOR_FOCUS    2
#define WIDGETCOLOR          1
#define WIDGETCOLOR_FWIN     2
#define WIDGETCOLOR_FOCUS    3
#define WIDGETCOLOR_DIALOG   4
#define WIDGETCOLOR_HLINK    5
#define DIALOGCOLOR          4
#define MENUCOLOR            6
#define OPENMENUCOLOR        7
#define ACTIVEMENUITEMCOLOR  7
#define ICONCOLOR            8

/* The process list fits the screen */
#define PROCESSLIST_CONF_HEIGHT 10

#include "conio-card10.h"

#endif /* CONTIKI_CONF_H_ */
