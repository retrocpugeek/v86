/*
 * A conio character screen on the card10's display, for CTK's drawing
 * (ctk-conio.c): 40 x 13 cells of 4 x 6 pixels (the X11 4x6 font) on the
 * 160 x 80 display, in colour.
 *
 * Cells are drawn into a buffer; conio_flush() draws the ones that changed
 * into the framebuffer and hands it to Epicardium. Besides the font's
 * characters, a cell can be
 *   - a line (CH_ULCORNER ... CH_VLINE, contiki-conf.h): window frames;
 *   - an icon tile (0x80 + n): a 4 x 6 piece of an icon (assets.h), as CTK
 *     draws an icon as 3 x 3 characters;
 *   - a space in the screen's colour, or text in ICONCOLOR, over the
 *     desktop's backdrop (assets.h).
 */
#include <stdint.h>
#include <string.h>

#include "epicardium.h"
#include "contiki-conf.h"
#include "assets.h"

#define RGB(r, g, b) ((((r) >> 3) << 11) | (((g) >> 2) << 5) | ((b) >> 3))
#define T ASSET_TRANSPARENT

/* Each colour index's text and background (T: the backdrop) */
static const struct { uint16_t fg, bg; } palette[] = {
  [SCREENCOLOR]         = { RGB(214, 168, 64),  T },
  [WINDOWCOLOR]         = { RGB(90, 84, 100),   RGB(196, 192, 204) },
  [WINDOWCOLOR_FOCUS]   = { RGB(24, 16, 32),    RGB(240, 236, 228) },
  [WIDGETCOLOR_FOCUS]   = { RGB(92, 48, 104),   RGB(240, 236, 228) },
  [WIDGETCOLOR_DIALOG]  = { RGB(24, 16, 32),    RGB(248, 226, 160) },
  [WIDGETCOLOR_HLINK]   = { RGB(40, 70, 170),   RGB(240, 236, 228) },
  [MENUCOLOR]           = { RGB(92, 48, 104),   RGB(248, 226, 160) },
  [OPENMENUCOLOR]       = { RGB(24, 16, 32),    RGB(248, 226, 160) },
  [ICONCOLOR]           = { RGB(248, 233, 190), T },
};

struct cell {
  char c;
  uint8_t colour;
  uint8_t reversed;
};

static struct cell screen[CONIO_HEIGHT][CONIO_WIDTH];
static struct cell shown[CONIO_HEIGHT][CONIO_WIDTH];
static uint8_t cursx, cursy, colour, reversed;
static int first = 1;
static union disp_framebuffer fb;

static void
put(char c)
{
  if(cursx < CONIO_WIDTH && cursy < CONIO_HEIGHT) {
    screen[cursy][cursx].c = c;
    screen[cursy][cursx].colour = colour;
    screen[cursy][cursx].reversed = reversed;
  }
  ++cursx;
}

unsigned char wherex(void) { return cursx; }
unsigned char wherey(void) { return cursy; }
void gotoxy(unsigned char x, unsigned char y) { cursx = x; cursy = y; }
unsigned char bgcolor(unsigned char c) { (void)c; return 0; }
unsigned char bordercolor(unsigned char c) { (void)c; return 0; }
unsigned char textcolor(unsigned char c) { unsigned char o = colour; colour = c; return o; }
unsigned char revers(unsigned char c) { unsigned char o = reversed; reversed = c; return o; }

void
screensize(unsigned char *x, unsigned char *y)
{
  *x = CONIO_WIDTH;
  *y = CONIO_HEIGHT;
}

void
clrscr(void)
{
  unsigned char y;
  for(y = 0; y < CONIO_HEIGHT; ++y) {
    cclearxy(0, y, CONIO_WIDTH);
  }
  gotoxy(0, 0);
}

void cputc(char c) { put(c); }

void
cputs(const char *str)
{
  while(*str) {
    put(*str++);
  }
}

void
cclear(unsigned char length)
{
  while(length--) {
    put(' ');
  }
}

void
chline(unsigned char length)
{
  while(length--) {
    put(CH_HLINE);
  }
}

void
cvline(unsigned char length)
{
  while(length--) {
    put(CH_VLINE);
    --cursx;
    ++cursy;
  }
}

void cclearxy(unsigned char x, unsigned char y, unsigned char l) { gotoxy(x, y); cclear(l); }
void chlinexy(unsigned char x, unsigned char y, unsigned char l) { gotoxy(x, y); chline(l); }
void cvlinexy(unsigned char x, unsigned char y, unsigned char l) { gotoxy(x, y); cvline(l); }
void cputsxy(unsigned char x, unsigned char y, const char *s) { gotoxy(x, y); cputs(s); }
void cputcxy(unsigned char x, unsigned char y, char c) { gotoxy(x, y); cputc(c); }

static void
pixel(int x, int y, uint16_t c)
{
  /* (Epicardium's framebuffer starts at the display's other corner) */
  x = DISP_WIDTH - 1 - x;
  y = DISP_HEIGHT - 1 - y;
  fb.fb[y][x][0] = c >> 8;
  fb.fb[y][x][1] = c & 0xff;
}

/* Is the cell's pixel (x, y) (0..3, 0..5) part of the line character c? */
static int
line_at(unsigned char c, int x, int y)
{
  /* (vertical lines at x 1, horizontal ones at y 2, as box drawing does) */
  int v = x == 1, h = y == 2;
  switch(c) {
  case CH_HLINE:    return h;
  case CH_VLINE:    return v;
  case CH_ULCORNER: return (h && x >= 1) || (v && y >= 2);
  case CH_URCORNER: return (h && x <= 1) || (v && y >= 2);
  case CH_LLCORNER: return (h && x >= 1) || (v && y <= 2);
  case CH_LRCORNER: return (h && x <= 1) || (v && y <= 2);
  }
  return 0;
}

static void
draw_cell(int cx, int cy, const struct cell *cell)
{
  unsigned char c = (unsigned char)cell->c;
  uint16_t fg = palette[cell->colour].fg, bg = palette[cell->colour].bg;
  int x0 = cx * 4, y0 = cy * 6, x, y;

  if(cell->reversed) {
    uint16_t t = fg;
    fg = bg == T ? RGB(24, 16, 32) : bg;
    bg = t;
  }
  for(y = 0; y < 6; ++y) {
    for(x = 0; x < 4; ++x) {
      uint16_t p;
      int on;
      if(c >= 0x80) {
        /* an icon's tile; a focused icon gets a gold background */
        p = tiles[c - 0x80][y][x];
        if(p == T) {
          p = cell->reversed ? RGB(214, 168, 64) : backdrop[y0 + y][x0 + x];
        }
        pixel(x0 + x, y0 + y, p);
        continue;
      }
      if(c >= CH_ULCORNER && c <= CH_VLINE) {
        on = line_at(c, x, y);
      } else if(c >= ' ' && c < 0x7f) {
        on = font4x6[c - ' '][y] & (0x80 >> x);
      } else {
        on = 0;
      }
      p = on ? fg : bg;
      pixel(x0 + x, y0 + y, p == T ? backdrop[y0 + y][x0 + x] : p);
    }
  }
}

int
conio_flush(void)
{
  int cx, cy, x, y, changed = 0;

  if(first) {
    /* (below the last row: the backdrop) */
    for(y = CONIO_HEIGHT * 6; y < DISP_HEIGHT; ++y) {
      for(x = 0; x < DISP_WIDTH; ++x) {
        pixel(x, y, backdrop[y][x]);
      }
    }
  }
  for(cy = 0; cy < CONIO_HEIGHT; ++cy) {
    for(cx = 0; cx < CONIO_WIDTH; ++cx) {
      if(first || memcmp(&screen[cy][cx], &shown[cy][cx], sizeof(struct cell))) {
        draw_cell(cx, cy, &screen[cy][cx]);
        shown[cy][cx] = screen[cy][cx];
        changed = 1;
      }
    }
  }
  first = 0;
  if(changed) {
    epic_disp_framebuffer(&fb);
  }
  return changed;
}
