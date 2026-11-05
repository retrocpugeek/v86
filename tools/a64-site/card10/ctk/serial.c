/*
 * A serial terminal on the desktop: the badge's console (UART0, the page's
 * console in the emulator), as core 1 sees it through Epicardium on core 0.
 *
 * Both ways between the cores:
 *   - in: a character arriving on the UART makes core 0 raise
 *     EPIC_INT_UART_RX on core 1 (through timer 5); its handler here,
 *     epic_isr_uart_rx(), takes what has come with epic_uart_read_str()
 *     (an API call back to core 0) and polls serial_process, which shows it
 *     and echoes it back;
 *   - out: the echo and the buttons' text go to core 0 with
 *     epic_uart_write_str().
 *
 * serial_process runs from the start (so the console's input is taken even
 * while the window is closed); the desktop opens the window with
 * serial_open().
 */
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "epicardium.h"
#include "contiki.h"
#include "ctk/ctk.h"
#include "serial.h"

#define COLS 36
#define ROWS 6

PROCESS(serial_process, "Serial");

/*---------------------------------------------------------------------------*/
/* What the interrupt handler takes, for serial_process (a ring: the handler
   only moves head, the process only tail) */
static volatile char rx[256];
static volatile uint8_t rx_head, rx_tail;
static volatile uint32_t irqs, bytes_in;

void
epic_isr_uart_rx(void)
{
  char buf[32];
  int n, i;

  ++irqs;
  /* (an API call from the handler: safe, as core 1's calls run with
     interrupts off, so none is under way) */
  while((n = epic_uart_read_str(buf, sizeof(buf))) > 0) {
    for(i = 0; i < n; ++i) {
      uint8_t next = rx_head + 1;
      if(next != rx_tail) {
        rx[rx_head] = buf[i];
        rx_head = next;
      }
    }
    bytes_in += n;
  }
  process_poll(&serial_process);
}

/*---------------------------------------------------------------------------*/
/* The window */
static struct ctk_window window;
static char text[ROWS * COLS + 1];
static char status[COLS + 1];
static struct ctk_label text_label = {CTK_LABEL(0, 0, COLS, ROWS, text)};
static struct ctk_label status_label = {CTK_LABEL(0, ROWS, COLS, 1, status)};
static struct ctk_button hello_button = {CTK_BUTTON(0, ROWS + 1, 5, "Hello")};
static struct ctk_button time_button = {CTK_BUTTON(9, ROWS + 1, 4, "Time")};
static struct ctk_button clear_button = {CTK_BUTTON(17, ROWS + 1, 5, "Clear")};
static uint8_t col, row, in_escape, after_cr;
static uint32_t bytes_out;
static process_event_t open_event;

static void
send(const char *s, size_t n)
{
  if(n) {
    epic_uart_write_str(s, n);
    bytes_out += n;
  }
}

static void
newline(void)
{
  col = 0;
  if(++row == ROWS) {
    row = ROWS - 1;
    memmove(text, text + COLS, (ROWS - 1) * COLS);
    memset(text + (ROWS - 1) * COLS, ' ', COLS);
  }
}

/* A character from the console: onto the screen, and its echo into out */
static void
receive(char c, char *out, size_t *n)
{
  if(in_escape) {
    /* (an escape sequence, an arrow key say: dropped, up to its letter) */
    if((c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || c == '~') {
      in_escape = 0;
    }
    return;
  }
  if(c == '\n' && after_cr) {
    after_cr = 0;
    return;
  }
  after_cr = c == '\r';
  if(c == '\r' || c == '\n') {
    newline();
    out[(*n)++] = '\r';
    out[(*n)++] = '\n';
  } else if(c == 0x7f || c == '\b') {
    if(col > 0) {
      text[row * COLS + --col] = ' ';
      out[(*n)++] = '\b';
      out[(*n)++] = ' ';
      out[(*n)++] = '\b';
    }
  } else if(c == 0x1b) {
    in_escape = 1;
  } else if(c >= ' ' && c < 0x7f) {
    text[row * COLS + col] = c;
    out[(*n)++] = c;
    if(++col == COLS) {
      newline();
    }
  }
}

static void
update_status(void)
{
  snprintf(status, sizeof(status), "IRQs %lu  in %lu  out %lu",
           (unsigned long)irqs, (unsigned long)bytes_in,
           (unsigned long)bytes_out);
  memset(status + strlen(status), ' ', COLS - strlen(status));
}

static void
clear(void)
{
  memset(text, ' ', ROWS * COLS);
  col = row = 0;
}

void
serial_open(void)
{
  process_post(&serial_process, open_event, NULL);
}

PROCESS_THREAD(serial_process, ev, data)
{
  PROCESS_BEGIN();
  open_event = process_alloc_event();
  clear();
  update_status();
  ctk_window_new(&window, COLS, ROWS + 2, "Serial: core 0's UART");
  CTK_WIDGET_ADD(&window, &text_label);
  CTK_WIDGET_ADD(&window, &status_label);
  CTK_WIDGET_ADD(&window, &hello_button);
  CTK_WIDGET_ADD(&window, &time_button);
  CTK_WIDGET_ADD(&window, &clear_button);
  CTK_WIDGET_FOCUS(&window, &hello_button);
  epic_interrupt_enable(EPIC_INT_UART_RX);

  while(1) {
    PROCESS_WAIT_EVENT();
    if(ev == PROCESS_EVENT_POLL) {
      /* what the interrupt handler took */
      char out[64];
      size_t n = 0;
      while(rx_tail != rx_head) {
        receive(rx[rx_tail], out, &n);
        rx_tail = rx_tail + 1;
        if(n > sizeof(out) - 3) {
          send(out, n);
          n = 0;
        }
      }
      send(out, n);
      update_status();
      CTK_WIDGET_REDRAW(&text_label);
      CTK_WIDGET_REDRAW(&status_label);
    } else if(ev == open_event) {
      ctk_window_open(&window);
    } else if(ev == ctk_signal_window_close) {
      ctk_window_close(&window);
    } else if(ev == ctk_signal_button_activate) {
      char line[64];
      if(data == (process_data_t)&hello_button) {
        static const char hello[] =
          "\r\nHello from core 1: Contiki's desktop on the MAX32666\r\n";
        send(hello, sizeof(hello) - 1);
      } else if(data == (process_data_t)&time_button) {
        uint32_t s = epic_rtc_get_seconds();
        int n = snprintf(line, sizeof(line),
                         "\r\nRTC %02lu:%02lu:%02lu, core 1 up %lu s\r\n",
                         (unsigned long)(s / 3600 % 24),
                         (unsigned long)(s / 60 % 60), (unsigned long)(s % 60),
                         clock_seconds());
        send(line, n);
      } else if(data == (process_data_t)&clear_button) {
        clear();
        CTK_WIDGET_REDRAW(&text_label);
      }
      update_status();
      CTK_WIDGET_REDRAW(&status_label);
    }
  }
  PROCESS_END();
}
