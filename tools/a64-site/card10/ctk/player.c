/*
 * An animation player on the desktop, the work shared between the cores:
 * core 0 reads each frame from the file system and decodes it (Epicardium's
 * animation decoder, epicardium-anim.patch: a task of its own), core 1
 * shows it.
 *
 *   - core 1 asks for a frame with epic_anim_decode(buffer), which returns
 *     at once;
 *   - core 0's decoder task reads and decodes the frame into that buffer,
 *     then raises EPIC_INT_ANIM_FRAME; its handler here,
 *     epic_isr_anim_frame(), polls player_process;
 *   - at each of the animation's ticks, player_process shows the frame
 *     that's ready (conio_video(): the window's CH_VIDEO cells) and asks for
 *     the next one in the other buffer (double buffering), so core 0 decodes
 *     the next frame while core 1 draws this one. A frame that isn't ready
 *     at its tick is counted late.
 *
 * The animations are the .rla files in /apps/ctk/anims (gen-anims.py makes
 * them); Next goes to the next one.
 */
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "epicardium.h"
#include "contiki.h"
#include "ctk/ctk.h"
#include "player.h"

#define DIR "/apps/ctk/anims"
/* The picture, in cells and pixels */
#define VW 24
#define VH 7
#define PW (VW * 4)
#define PH (VH * 6)

PROCESS(player_process, "Player");

/*---------------------------------------------------------------------------*/
/* The frames core 0 decodes into: one shown, one being decoded */
static uint16_t frames[2][PW * PH];
static volatile uint8_t ready;
static volatile uint32_t irqs;

void
epic_isr_anim_frame(void)
{
  ++irqs;
  ready = 1;
  process_poll(&player_process);
}

/*---------------------------------------------------------------------------*/
static struct ctk_window window;
static char picture[VW * VH + 1];
static char status[VW + 1];
static struct ctk_label picture_label = {CTK_LABEL(0, 0, VW, VH, picture)};
static struct ctk_label status_label = {CTK_LABEL(0, VH, VW, 1, status)};
static struct ctk_button next_button = {CTK_BUTTON(0, VH + 1, 4, "Next")};
static struct ctk_button pause_button = {CTK_BUTTON(7, VH + 1, 5, "Pause")};
static struct ctk_button close_button = {CTK_BUTTON(16, VH + 1, 5, "Close")};

static struct etimer tick;
static struct anim_info info;
static char name[EPICSTAT_MAX_PATH + 1];
static int which = -1;
static uint8_t decoding, playing, paused;
static uint32_t late;
static process_event_t open_event;

static void
update_status(void)
{
  if(!playing) {
    snprintf(status, sizeof(status), "%s", name[0] ? name : "no animations");
  } else {
    snprintf(status, sizeof(status), "%-8.8s %2u/s late %lu",
             name, info.fps, (unsigned long)late);
  }
  memset(status + strlen(status), ' ', VW - strlen(status));
  CTK_WIDGET_REDRAW(&status_label);
}

/* The video's place on the screen: where CTK draws the picture label */
static void
show(const uint16_t *frame)
{
  conio_video(frame, window.x + 1 + picture_label.x,
              window.y + 1 + CTK_CONF_MENUS + picture_label.y, VW, VH);
}

/* Opens the n-th animation in DIR (wrapping round); returns whether one
   opened */
static int
open_nth(int n)
{
  struct epic_stat st;
  char path[sizeof(DIR) + 1 + EPICSTAT_MAX_PATH + 1];
  int count = 0, pass, fd;

  playing = 0;
  name[0] = '\0';
  /* (twice round, if n is past the last) */
  for(pass = 0; pass < 2; ++pass) {
    fd = epic_file_opendir(DIR);
    if(fd < 0) {
      return 0;
    }
    while(epic_file_readdir(fd, &st) == 0 && st.type != EPICSTAT_NONE) {
      size_t l = strlen(st.name);
      if(st.type != EPICSTAT_FILE || l < 4 || strcmp(st.name + l - 4, ".rla")) {
        continue;
      }
      if(count++ == n) {
        epic_file_close(fd);
        which = n;
        snprintf(path, sizeof(path), "%s/%s", DIR, st.name);
        snprintf(name, sizeof(name), "%.*s", (int)(l - 4), st.name);
        if(epic_anim_open(path, &info) != 0 || info.width != PW ||
           info.height != PH || info.fps == 0) {
          return 0;
        }
        playing = 1;
        return 1;
      }
    }
    epic_file_close(fd);
    if(count == 0) {
      return 0;
    }
    n %= count;
    count = 0;
  }
  return 0;
}

static void
start(int n)
{
  late = 0;
  ready = 0;
  /* (a frame still coming for the last animation: wait for it, in
     epic_anim_open, which finishes it first) */
  if(open_nth(n)) {
    decoding = 0;
    epic_anim_decode(frames[decoding]);
    etimer_set(&tick, CLOCK_SECOND / info.fps);
  } else {
    etimer_stop(&tick);
  }
  update_status();
}

void
player_open(void)
{
  process_post(&player_process, open_event, NULL);
}

PROCESS_THREAD(player_process, ev, data)
{
  PROCESS_BEGIN();
  open_event = process_alloc_event();
  memset(picture, CH_VIDEO, VW * VH);
  ctk_window_new(&window, VW, VH + 2, "Player (core 0 decodes)");
  CTK_WIDGET_ADD(&window, &picture_label);
  CTK_WIDGET_ADD(&window, &status_label);
  CTK_WIDGET_ADD(&window, &next_button);
  CTK_WIDGET_ADD(&window, &pause_button);
  CTK_WIDGET_ADD(&window, &close_button);
  CTK_WIDGET_FOCUS(&window, &next_button);
  epic_interrupt_enable(EPIC_INT_ANIM_FRAME);

  while(1) {
    PROCESS_WAIT_EVENT();
    if(ev == open_event) {
      ctk_window_open(&window);
      show(NULL);
      if(!playing) {
        paused = 0;
        start(which < 0 ? 0 : which);
      }
    } else if(ev == PROCESS_EVENT_TIMER && data == &tick) {
      if(paused || !playing) {
        continue;
      }
      etimer_reset(&tick);
      if(ready) {
        /* the frame core 0 has just decoded: show it, and have core 0
           decode the next into the other buffer meanwhile */
        ready = 0;
        show(frames[decoding]);
        decoding ^= 1;
        epic_anim_decode(frames[decoding]);
      } else {
        ++late;
        update_status();
      }
    } else if(ev == ctk_signal_button_activate) {
      if(data == (process_data_t)&next_button) {
        start(which + 1);
      } else if(data == (process_data_t)&pause_button) {
        paused = !paused;
        if(!paused && playing) {
          etimer_restart(&tick);
        }
      } else if(data == (process_data_t)&close_button) {
        goto close;
      }
    } else if(ev == ctk_signal_window_close) {
      goto close;
    }
    continue;
  close:
    epic_anim_close();
    etimer_stop(&tick);
    playing = 0;
    ctk_window_close(&window);
    show(NULL);
  }
  PROCESS_END();
}
