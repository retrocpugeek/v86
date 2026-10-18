// Tetris for the CH32V003, played over USART1 (115200 8N1) on an ANSI
// terminal: 10 x 20 board, 7-bag randomizer, ghost piece, levels.
//
// Keys: left/right or a/d move, up or w rotate (z: the other way), down or
// s soft drop, space hard drop, p pause, r restart.
//
// Runs from SysTick: a 10 ms interrupt; the main loop sleeps (wfi) between
// ticks. Only changed cells are redrawn.
#include <stdint.h>
#include "ch32v003.h"

#define W 10
#define H 20
#define TICK_MS 10

// Screen layout (1-based terminal rows and columns)
#define BOARD_ROW 3
#define BOARD_COL 3
#define PANEL_COL (BOARD_COL + 2 * W + 4)

static volatile uint32_t ticks;

// ---- hardware ----

static void putc_(char c)
{
    while(!(USART1_STATR & USART_TXE));
    USART1_DATAR = (uint8_t)c;
}

static void puts_(const char *s)
{
    while(*s) putc_(*s++);
}

static void putnum(uint32_t n)
{
    char b[11];
    int i = 0;
    do
    {
        b[i++] = '0' + n % 10;
        n /= 10;
    }
    while(n);
    while(i) putc_(b[--i]);
}

static int getc_(void)
{
    return USART1_STATR & USART_RXNE ? (int)(USART1_DATAR & 0xff) : -1;
}

__attribute__((interrupt("machine"), aligned(4)))
void trap_handler(void)
{
    uint32_t cause;
    __asm__ volatile("csrr %0, mcause" : "=r"(cause));
    if(cause == (0x80000000u | IRQ_SYSTICK))
    {
        STK_SR = 0;
        ticks++;
    }
}

static void hw_init(void)
{
    // HCLK = SYSCLK = HSI, 24 MHz (the reset value divides by 3)
    RCC_CFGR0 &= ~0xf0u;
    RCC_APB2PCENR |= RCC_AFIOEN | RCC_IOPDEN | RCC_USART1EN;
    // PD5: alternate function push-pull, 10 MHz (USART1 TX); PD6: floating
    // input (RX)
    GPIOD_CFGLR = (GPIOD_CFGLR & ~(0xffu << 20)) | 0x9u << 20 | 0x4u << 24;
    USART1_BRR = HCLK_HZ / 115200;
    USART1_CTLR1 = USART_UE | USART_TE | USART_RE;
    // SysTick: HCLK / 8, an interrupt every TICK_MS
    STK_CTLR = 0;
    STK_SR = 0;
    STK_CNTL = 0;
    STK_CMPLR = HCLK_HZ / 8 / 1000 * TICK_MS;
    STK_CTLR = STK_STE | STK_STIE | STK_STRE;
    PFIC_IENR1 = 1u << IRQ_SYSTICK;
    __asm__ volatile("csrs mstatus, %0" ::"r"(0x88));
}

static void wait_tick(void)
{
    uint32_t t = ticks;
    while(ticks == t) __asm__ volatile("wfi");
}

// ---- terminal ----

static void at(int row, int col)
{
    putc_('\033');
    putc_('[');
    putnum(row);
    putc_(';');
    putnum(col);
    putc_('H');
}

static void color(int bg)
{
    puts_("\033[0;");
    if(bg >= 0)
    {
        puts_("48;5;");
        putnum(bg);
    }
    putc_('m');
}

// ---- pieces ----

// 4x4 masks per rotation, row-major from bit 15
static const uint16_t shapes[7][4] = {
    {0x0f00, 0x2222, 0x00f0, 0x4444}, // I
    {0x6600, 0x6600, 0x6600, 0x6600}, // O
    {0x4e00, 0x4640, 0x0e40, 0x4c40}, // T
    {0x6c00, 0x4620, 0x06c0, 0x8c40}, // S
    {0xc600, 0x2640, 0x0c60, 0x4c80}, // Z
    {0x8e00, 0x6440, 0x0e20, 0x44c0}, // J
    {0x2e00, 0x4460, 0x0e80, 0xc440}, // L
};

// Cell contents: 0 empty, 1..7 a piece's color, GHOST the drop preview
#define GHOST 8
// 256-color palette entries for 0..8
static const uint8_t palette[9] = {234, 51, 226, 201, 46, 196, 27, 208, 240};

static uint8_t board[H][W];
// What the terminal shows
static uint8_t shown[H][W];
static int8_t px, py, prot;
static uint8_t piece, next_piece;
static uint8_t bag[7], bag_left;
static uint32_t score, lines, level;
static uint32_t rng = 1;
static uint8_t over, paused, started;
static uint32_t shown_score = ~0u, shown_lines = ~0u, shown_level = ~0u;
static int8_t shown_next = -1, shown_state = -1;

static uint32_t random_(void)
{
    rng ^= rng << 13;
    rng ^= rng >> 17;
    rng ^= rng << 5;
    return rng;
}

static uint8_t from_bag(void)
{
    if(!bag_left)
    {
        for(int i = 0; i < 7; i++) bag[i] = i;
        bag_left = 7;
    }
    int i = random_() % bag_left;
    uint8_t p = bag[i];
    bag[i] = bag[--bag_left];
    return p;
}

static int cell(int p, int rot, int r, int c) { return shapes[p][rot & 3] >> (15 - (r * 4 + c)) & 1; }

static int fits(int p, int rot, int x, int y)
{
    for(int r = 0; r < 4; r++)
    {
        for(int c = 0; c < 4; c++)
        {
            if(!cell(p, rot, r, c)) continue;
            int bx = x + c, by = y + r;
            if(bx < 0 || bx >= W || by >= H) return 0;
            if(by >= 0 && board[by][bx]) return 0;
        }
    }
    return 1;
}

static void spawn(void)
{
    piece = next_piece;
    next_piece = from_bag();
    px = 3;
    py = piece == 0 ? -1 : 0;
    prot = 0;
    if(!fits(piece, prot, px, py)) over = 1;
}

static void new_game(void)
{
    for(int r = 0; r < H; r++)
        for(int c = 0; c < W; c++) board[r][c] = 0;
    score = lines = 0;
    level = 1;
    over = paused = 0;
    bag_left = 0;
    next_piece = from_bag();
    spawn();
}

static int drop_interval(void)
{
    // Ticks per row: 0.5 s at level 1, faster by 40 ms a level, down to 60 ms
    int t = 50 - 4 * (int)(level - 1);
    return t < 6 ? 6 : t;
}

static void lock_piece(void)
{
    for(int r = 0; r < 4; r++)
    {
        for(int c = 0; c < 4; c++)
        {
            if(cell(piece, prot, r, c) && py + r >= 0) board[py + r][px + c] = piece + 1;
        }
    }
    int cleared = 0;
    for(int r = H - 1; r >= 0; r--)
    {
        int full = 1;
        for(int c = 0; c < W; c++) full &= board[r][c] != 0;
        if(!full) continue;
        for(int k = r; k > 0; k--)
            for(int c = 0; c < W; c++) board[k][c] = board[k - 1][c];
        for(int c = 0; c < W; c++) board[0][c] = 0;
        cleared++;
        r++;
    }
    static const uint16_t points[5] = {0, 100, 300, 500, 800};
    score += points[cleared] * level;
    lines += cleared;
    level = lines / 10 + 1;
    spawn();
}

static void move(int dx)
{
    if(fits(piece, prot, px + dx, py)) px += dx;
}

static void rotate(int dir)
{
    // Simple wall kicks: in place, one or two columns either way, one row up
    static const int8_t kicks[][2] = {{0, 0}, {-1, 0}, {1, 0}, {-2, 0}, {2, 0}, {0, -1}};
    int rot = (prot + dir) & 3;
    for(unsigned i = 0; i < sizeof kicks / sizeof kicks[0]; i++)
    {
        if(fits(piece, rot, px + kicks[i][0], py + kicks[i][1]))
        {
            px += kicks[i][0];
            py += kicks[i][1];
            prot = rot;
            return;
        }
    }
}

// Returns 0 if the piece locked
static int soft_drop(void)
{
    if(fits(piece, prot, px, py + 1))
    {
        py++;
        return 1;
    }
    lock_piece();
    return 0;
}

static void hard_drop(void)
{
    int n = 0;
    while(fits(piece, prot, px, py + 1))
    {
        py++;
        n++;
    }
    score += 2 * n;
    lock_piece();
}

// ---- drawing ----

static void draw_frame(void)
{
    puts_("\033[0m\033[2J\033[?25l");
    at(1, BOARD_COL);
    puts_("\033[1mCH32V003 TETRIS\033[0m");
    for(int r = 0; r <= H + 1; r++)
    {
        int row = BOARD_ROW - 1 + r;
        color(244);
        at(row, BOARD_COL - 2);
        puts_("  ");
        at(row, BOARD_COL + 2 * W);
        puts_("  ");
        if(r == 0 || r == H + 1)
        {
            at(row, BOARD_COL);
            for(int c = 0; c < W; c++) puts_("  ");
        }
    }
    color(-1);
    at(BOARD_ROW, PANEL_COL);
    puts_("NEXT");
    at(BOARD_ROW + 6, PANEL_COL);
    puts_("SCORE");
    at(BOARD_ROW + 9, PANEL_COL);
    puts_("LINES");
    at(BOARD_ROW + 12, PANEL_COL);
    puts_("LEVEL");
    for(int r = 0; r < H; r++)
        for(int c = 0; c < W; c++) shown[r][c] = 0xff;
    shown_score = shown_lines = shown_level = ~0u;
    shown_next = shown_state = -1;
}

static void draw_number(int row, uint32_t v, uint32_t *shown_v)
{
    if(v == *shown_v) return;
    *shown_v = v;
    color(-1);
    at(row, PANEL_COL);
    putnum(v);
    puts_("      ");
}

static void draw(void)
{
    // The board with the ghost and the falling piece
    uint8_t want[H][W];
    for(int r = 0; r < H; r++)
        for(int c = 0; c < W; c++) want[r][c] = board[r][c];
    if(!over && started)
    {
        int gy = py;
        while(fits(piece, prot, px, gy + 1)) gy++;
        for(int r = 0; r < 4; r++)
        {
            for(int c = 0; c < 4; c++)
            {
                if(!cell(piece, prot, r, c)) continue;
                if(gy + r >= 0 && !want[gy + r][px + c]) want[gy + r][px + c] = GHOST;
                if(py + r >= 0) want[py + r][px + c] = piece + 1;
            }
        }
    }
    int last = -1;
    for(int r = 0; r < H; r++)
    {
        for(int c = 0; c < W; c++)
        {
            uint8_t v = want[r][c];
            if(v == shown[r][c]) continue;
            shown[r][c] = v;
            if(v != last)
            {
                color(palette[v]);
                last = v;
            }
            at(BOARD_ROW + r, BOARD_COL + 2 * c);
            puts_(v == GHOST ? "[]" : "  ");
        }
    }
    if(next_piece != shown_next)
    {
        shown_next = next_piece;
        for(int r = 0; r < 2; r++)
        {
            at(BOARD_ROW + 2 + r, PANEL_COL);
            for(int c = 0; c < 4; c++)
            {
                color(cell(next_piece, 0, r, c) ? palette[next_piece + 1] : -1);
                puts_("  ");
            }
        }
    }
    draw_number(BOARD_ROW + 7, score, &shown_score);
    draw_number(BOARD_ROW + 10, lines, &shown_lines);
    draw_number(BOARD_ROW + 13, level, &shown_level);
    int state = !started ? 0 : over ? 1 : paused ? 2 : 3;
    if(state != shown_state)
    {
        shown_state = state;
        static const char *const msg[4] = {
            "\033[1mPRESS ANY KEY\033[0m",
            "\033[1;31mGAME OVER\033[0m R: again",
            "\033[1;33mPAUSED\033[0m P: go on",
            "",
        };
        color(-1);
        at(BOARD_ROW + 16, PANEL_COL);
        puts_(msg[state]);
        puts_("\033[0K");
        at(BOARD_ROW + 18, PANEL_COL);
        puts_(state == 3 ? "\033[2m< > move  ^ rotate\033[0K" : "\033[0K");
        at(BOARD_ROW + 19, PANEL_COL);
        puts_(state == 3 ? "\033[2mv drop  _ hard drop\033[0K" : "\033[0K");
    }
    color(-1);
    at(BOARD_ROW + H + 2, 1);
}

// ---- input ----

enum { K_NONE, K_LEFT, K_RIGHT, K_ROT, K_ROT_BACK, K_DOWN, K_DROP, K_PAUSE, K_RESTART, K_REDRAW, K_OTHER };

// Arrow keys arrive as ESC [ A..D
static int key(void)
{
    static uint8_t esc;
    for(;;)
    {
        int ch = getc_();
        if(ch < 0) return K_NONE;
        if(esc == 1)
        {
            esc = ch == '[' || ch == 'O' ? 2 : 0;
            continue;
        }
        if(esc == 2)
        {
            esc = 0;
            switch(ch)
            {
                case 'A': return K_ROT;
                case 'B': return K_DOWN;
                case 'C': return K_RIGHT;
                case 'D': return K_LEFT;
                default: continue;
            }
        }
        switch(ch)
        {
            case 27: esc = 1; continue;
            case 'a': case 'A': case 'h': return K_LEFT;
            case 'd': case 'D': case 'l': return K_RIGHT;
            case 'w': case 'W': case 'k': case 'x': case 'X': return K_ROT;
            case 'z': case 'Z': return K_ROT_BACK;
            case 's': case 'S': case 'j': return K_DOWN;
            case ' ': return K_DROP;
            case 'p': case 'P': return K_PAUSE;
            case 'r': case 'R': return K_RESTART;
            case 12: return K_REDRAW; // ^L
            default: return K_OTHER;
        }
    }
}

int main(void)
{
    hw_init();
    draw_frame();
    draw();
    int fall = 0;
    for(;;)
    {
        wait_tick();
        int k;
        while((k = key()) != K_NONE)
        {
            if(k == K_REDRAW)
            {
                draw_frame();
                continue;
            }
            if(!started || over)
            {
                if(started && k != K_RESTART) continue;
                // The time of the first key seeds the randomizer
                rng = (rng ^ ticks * 2654435761u) | 1;
                started = 1;
                new_game();
                fall = 0;
                continue;
            }
            if(k == K_RESTART)
            {
                new_game();
                continue;
            }
            if(k == K_PAUSE)
            {
                paused = !paused;
                continue;
            }
            if(paused) continue;
            switch(k)
            {
                case K_LEFT: move(-1); break;
                case K_RIGHT: move(1); break;
                case K_ROT: rotate(1); break;
                case K_ROT_BACK: rotate(-1); break;
                case K_DOWN:
                    if(soft_drop()) score++;
                    fall = 0;
                    break;
                case K_DROP: hard_drop(); fall = 0; break;
            }
        }
        if(started && !over && !paused && ++fall >= drop_interval())
        {
            fall = 0;
            soft_drop();
        }
        draw();
    }
}
