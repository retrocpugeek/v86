/*
 * Contiki's desktop, CTK, on the card10's core 1: an l0dable the badge's
 * menu starts (/apps/ctk, metadata.json), on Epicardium's API.
 *
 * This file is Contiki's platform for it (its clock, the main loop, keys
 * from the buttons) and the desktop: icons and an "RCG" menu for the
 * applications (Contiki's calculator, process list and about box, the
 * RetroCPUGeek window here, which names the chip it runs on: the card10's
 * Maxim MAX32666, a serial terminal on core 0's UART, serial.c, and an
 * animation player whose frames core 0 decodes, player.c), over
 * a backdrop of the RetroCPUGeek avatar's chip (conio-card10.c, assets.h).
 *
 * The buttons (the top left one stays the badge's own):
 *   bottom left / bottom right   previous / next widget (or menu item)
 *   top right                    activate it
 *   top right, held              open or close the menus
 *   bottom right, held           next window (or, in the menus, the next menu)
 *   bottom left, held            close the window in front (Ctrl-C)
 *
 * With a touch panel (epic_touch_read: the emulator's card10 has one), a
 * finger is CTK's mouse: tap a widget, an icon or a menu; tap a window's
 * title, then tap where it should go.
 */
#include <stdint.h>
#include <stdio.h>

#include "epicardium.h"
#include "contiki.h"
#include "ctk/ctk.h"
#include "assets.h"
#include "serial.h"
#include "player.h"

/* Contiki's applications used here */
PROCESS_NAME(calc_process);
PROCESS_NAME(about_process);
PROCESS_NAME(processes_process);
PROCESS_NAME(ctk_process);

/*---------------------------------------------------------------------------*/
/* The clock */
static uint64_t start_ms;

void clock_init(void) { start_ms = epic_rtc_get_milliseconds(); }
clock_time_t clock_time(void) { return (clock_time_t)(epic_rtc_get_milliseconds() - start_ms); }
unsigned long clock_seconds(void) { return clock_time() / 1000; }
rtimer_clock_t rtimer_arch_now(void) { return (rtimer_clock_t)clock_time(); }

/*---------------------------------------------------------------------------*/
/* Keys, from the buttons: a press is a key when it is let go, a hold (of
   HOLD_MS) is another key, at once */
#define HOLD_MS 500
static char keys[8];
static uint8_t key_head, key_tail;

static void
key_put(char c)
{
  uint8_t next = (key_head + 1) % sizeof(keys);
  if(next != key_tail) {
    keys[key_head] = c;
    key_head = next;
  }
}

unsigned char ctk_arch_keyavail(void) { return key_head != key_tail; }

char
ctk_arch_getkey(void)
{
  char c = 0;
  if(key_head != key_tail) {
    c = keys[key_tail];
    key_tail = (key_tail + 1) % sizeof(keys);
  }
  return c;
}

static const struct {
  uint8_t mask;
  char press, hold, press_in_menu, hold_in_menu;
} buttons[] = {
  { BUTTON_LEFT_BOTTOM,  CH_PREV_WIDGET, 3,                         CH_CURS_UP,   CH_CURS_LEFT },
  { BUTTON_RIGHT_BOTTOM, CH_NEXT_WIDGET, CTK_CONF_WINDOWSWITCH_KEY, CH_CURS_DOWN, CH_CURS_RIGHT },
  { BUTTON_RIGHT_TOP,    CH_ENTER,       CTK_CONF_MENU_KEY,         CH_ENTER,     CTK_CONF_MENU_KEY },
};
static clock_time_t down_at[3];
static uint8_t held[3];
/* Whether the menus are open, as the keys left them */
static uint8_t menus_open;

static void
poll_buttons(void)
{
  uint8_t now = epic_buttons_read(BUTTON_LEFT_BOTTOM | BUTTON_RIGHT_BOTTOM | BUTTON_RIGHT_TOP);
  unsigned i;

  for(i = 0; i < sizeof(buttons) / sizeof(buttons[0]); ++i) {
    int down = (now & buttons[i].mask) != 0;
    if(down && !down_at[i]) {
      down_at[i] = clock_time() | 1;
      held[i] = 0;
    } else if(down && !held[i] && clock_time() - down_at[i] >= HOLD_MS) {
      char c = menus_open ? buttons[i].hold_in_menu : buttons[i].hold;
      held[i] = 1;
      if(c == CTK_CONF_MENU_KEY) {
        menus_open = !menus_open;
      }
      key_put(c);
    } else if(!down && down_at[i]) {
      if(!held[i]) {
        char c = menus_open ? buttons[i].press_in_menu : buttons[i].press;
        if(c == CH_ENTER) {
          /* (choosing a menu item closes the menus) */
          menus_open = 0;
        }
        key_put(c);
      }
      down_at[i] = 0;
    }
  }
}

/*---------------------------------------------------------------------------*/
/* The mouse: the touch panel, read once each time round the main loop.
   Its points are finer than the display's pixels; CTK gets them in pixels
   (and in character cells, 4 x 6 pixels) */
static struct touch_state touch;

static void
poll_touch(void)
{
  if(epic_touch_read(&touch) != 0) {
    touch.down = 0;
  }
}

void ctk_mouse_init(void) {}
void ctk_mouse_show(void) {}
void ctk_mouse_hide(void) {}
unsigned short ctk_mouse_x(void) { return touch.x * DISP_WIDTH / TOUCH_WIDTH; }
unsigned short ctk_mouse_y(void) { return touch.y * DISP_HEIGHT / TOUCH_HEIGHT; }
unsigned char ctk_mouse_xtoc(unsigned short x) { return x / 4; }
unsigned char ctk_mouse_ytoc(unsigned short y) { return y / 6; }
unsigned char ctk_mouse_button(void) { return touch.down; }

/*---------------------------------------------------------------------------*/
/* The RetroCPUGeek window */
static struct ctk_window rcg_window;
static struct ctk_label rcg_lines[] = {
  {CTK_LABEL(0, 0, 24, 1, "RetroCPUGeek on MAX32666")},
  {CTK_LABEL(0, 1, 24, 1, "------------------------")},
  {CTK_LABEL(0, 2, 24, 1, "Contiki's desktop, CTK,")},
  {CTK_LABEL(0, 3, 24, 1, "on the card10's core 1,")},
  {CTK_LABEL(0, 4, 24, 1, "on Epicardium (FreeRTOS)")},
  {CTK_LABEL(0, 5, 24, 1, "github.com/retrocpugeek")},
};
static struct ctk_button rcg_close = {CTK_BUTTON(9, 7, 5, "Close")};

PROCESS(rcg_process, "RetroCPUGeek");

PROCESS_THREAD(rcg_process, ev, data)
{
  unsigned i;

  PROCESS_BEGIN();
  ctk_window_new(&rcg_window, 24, 8, "MAX32666");
  for(i = 0; i < sizeof(rcg_lines) / sizeof(rcg_lines[0]); ++i) {
    CTK_WIDGET_ADD(&rcg_window, &rcg_lines[i]);
  }
  CTK_WIDGET_ADD(&rcg_window, &rcg_close);
  CTK_WIDGET_FOCUS(&rcg_window, &rcg_close);
  ctk_window_open(&rcg_window);
  while(1) {
    PROCESS_WAIT_EVENT();
    if(ev == PROCESS_EVENT_EXIT || ev == ctk_signal_window_close ||
       (ev == ctk_signal_button_activate && data == (process_data_t)&rcg_close)) {
      ctk_window_close(&rcg_window);
      PROCESS_EXIT();
    }
  }
  PROCESS_END();
}

/*---------------------------------------------------------------------------*/
/* The desktop: icons and the RCG menu, each starting an application */
#define TILES(i) { 0x80 + 9 * (i), 0x81 + 9 * (i), 0x82 + 9 * (i), \
                   0x83 + 9 * (i), 0x84 + 9 * (i), 0x85 + 9 * (i), \
                   0x86 + 9 * (i), 0x87 + 9 * (i), 0x88 + 9 * (i) }
static char rcg_map[9] = TILES(ICON_RCG);
static char calc_map[9] = TILES(ICON_CALC);
static char play_map[9] = TILES(ICON_PLAY);
static char term_map[9] = TILES(ICON_TERM);
static struct ctk_icon rcg_icon = {CTK_ICON("RCG", NULL, rcg_map)};
static struct ctk_icon calc_icon = {CTK_ICON("Calc", NULL, calc_map)};
static struct ctk_icon play_icon = {CTK_ICON("Play", NULL, play_map)};
static struct ctk_icon term_icon = {CTK_ICON("Term", NULL, term_map)};

static struct ctk_menu rcg_menu;
enum { ITEM_RCG, ITEM_CALC, ITEM_PLAYER, ITEM_PROCS, ITEM_SERIAL, ITEM_ABOUT, ITEM_LINE, ITEM_QUIT };
static int quit;

PROCESS(desktop_process, "Desktop");

static void
launch(struct process *p)
{
  if(!process_is_running(p)) {
    process_start(p, NULL);
  }
}

PROCESS_THREAD(desktop_process, ev, data)
{
  PROCESS_BEGIN();
  /* (the first one goes to the bottom right) */
  CTK_ICON_ADD(&rcg_icon, &desktop_process);
  CTK_ICON_ADD(&calc_icon, &desktop_process);
  CTK_ICON_ADD(&play_icon, &desktop_process);
  CTK_ICON_ADD(&term_icon, &desktop_process);
  ctk_menu_new(&rcg_menu, "RCG");
  ctk_menuitem_add(&rcg_menu, "RetroCPUGeek");
  ctk_menuitem_add(&rcg_menu, "Calculator");
  ctk_menuitem_add(&rcg_menu, "Player");
  ctk_menuitem_add(&rcg_menu, "Processes");
  ctk_menuitem_add(&rcg_menu, "Serial terminal");
  ctk_menuitem_add(&rcg_menu, "About Contiki");
  ctk_menuitem_add(&rcg_menu, "-");
  ctk_menuitem_add(&rcg_menu, "Quit");
  ctk_menu_add(&rcg_menu);
  ctk_desktop_redraw(NULL);
  /* the brand, to start with */
  launch(&rcg_process);
  while(1) {
    PROCESS_WAIT_EVENT();
    if(ev == ctk_signal_widget_activate) {
      if(data == (process_data_t)&rcg_icon) launch(&rcg_process);
      else if(data == (process_data_t)&calc_icon) launch(&calc_process);
      else if(data == (process_data_t)&play_icon) player_open();
      else if(data == (process_data_t)&term_icon) serial_open();
    } else if(ev == ctk_signal_menu_activate && data == (process_data_t)&rcg_menu) {
      switch(rcg_menu.active) {
      case ITEM_RCG: launch(&rcg_process); break;
      case ITEM_CALC: launch(&calc_process); break;
      case ITEM_PLAYER: player_open(); break;
      case ITEM_PROCS: launch(&processes_process); break;
      case ITEM_SERIAL: serial_open(); break;
      case ITEM_ABOUT: launch(&about_process); break;
      case ITEM_QUIT: quit = 1; break;
      }
    }
  }
  PROCESS_END();
}

/*---------------------------------------------------------------------------*/
int
main(void)
{
  printf("Contiki %s, CTK on the card10\n", CONTIKI_VERSION_STRING);
  epic_disp_open();
  clock_init();
  process_init();
  process_start(&etimer_process, NULL);
  process_start(&ctk_process, NULL);
  process_start(&serial_process, NULL);
  process_start(&player_process, NULL);
  process_start(&desktop_process, NULL);

  while(!quit) {
    poll_buttons();
    poll_touch();
    /* (CTK polls itself: this runs everything that's ready once) */
    process_run();
    if(etimer_pending() && etimer_next_expiration_time() <= clock_time()) {
      etimer_request_poll();
    }
    conio_flush();
    epic_sleep(20);
  }
  epic_disp_close();
  return 0;
}
