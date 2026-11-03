// Tiny BASIC for the CH32V003, over USART1 (115200 8N1), with a
// Wozmon-style machine monitor (MON).
//
// Integers are 32-bit; variables are A to Z. Statements, several to a line
// with ':':
//   PRINT (or ?) items separated by ; or , ("text", expressions, HEX(x)),
//   LET (optional), IF x THEN statement or line, GOTO, GOSUB, RETURN,
//   FOR v = a TO b [STEP c], NEXT [v], INPUT ["prompt";] v[, v],
//   POKE address, value (a 32-bit store), DELAY ms, REM (or '), END, STOP,
//   CLS; and RUN, LIST, NEW, SAVE and LOAD (the program in the last 1 KB
//   of flash), MON.
// Expressions: + - * / MOD, = <> < > <= >= (true is -1), AND, OR, XOR and
// NOT (bitwise), $hex numbers, PEEK(address) (a 32-bit load), RND(n) (1 to
// n), ABS(x), TIME (ms since reset), FREE (program bytes left).
//
// The monitor, in hex: an address shows its word, a.b dumps words from a
// to b, a: v v ... stores words from a, a R calls a, Q (or Ctrl-C) goes
// back to BASIC.
//
// Ctrl-C stops a running program. A fault (e.g. PEEK of an address that
// isn't there) is reported, and returns to the prompt.
#include <stdint.h>
#include "ch32v003.h"

// The flash interface (CH32V003 reference manual, chapter 16)
#define FLASH_KEYR     REG(0x40022004)
#define FLASH_STATR    REG(0x4002200c)
#define FLASH_CTLR     REG(0x40022010)
#define FLASH_ADDR     REG(0x40022014)
#define FLASH_MODEKEYR REG(0x40022024)
#define FLASH_BSY      (1u << 0)
#define FLASH_PER      (1u << 1)
#define FLASH_STRT     (1u << 6)
#define FLASH_LOCK     (1u << 7)
#define FLASH_FLOCK    (1u << 15)
#define FLASH_FTPG     (1u << 16)
#define FLASH_BUFLOAD  (1u << 18)
#define FLASH_BUFRST   (1u << 19)

// The saved program: the last 1 KB of flash, a header (magic, length) and
// the program's lines
#define SAVE_ADDR  0x08003c00u
#define SAVE_MAGIC 0x31304254u // "TB01"
// The program's room: 888 bytes of the 2 KB of RAM, which leaves the stack
// 700 or so, for parentheses 7 deep
#define PROG_SIZE  888

#define LINE_MAX    72
#define FOR_DEPTH   6
#define GOSUB_DEPTH 8

// The program: lines in order, each [number (2 bytes)][length][text, 0]
static uint8_t prog[PROG_SIZE];
static int prog_len;
static int32_t var[26];

static char inbuf[LINE_MAX + 1];
static const char *tp;  // the text being run
static int lp = -1;     // its line (an offset in prog), or -1: typed in
static uint8_t jumped;  // the statement went to another line
static struct { uint8_t var; int32_t limit, step; int lp; const char *tp; } fors[FOR_DEPTH];
static uint8_t nfor;
static struct { int lp; const char *tp; } gosubs[GOSUB_DEPTH];
static uint8_t ngosub;
static uint8_t in_mon;
static uint32_t xam;    // the monitor's address
static uint32_t rng = 2463534242u;
static volatile uint32_t ms;
static uint32_t fault_cause, fault_pc, fault_val;

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

static void newline(void) { puts_("\r\n"); }

static void putnum(int32_t v)
{
    char b[11];
    int i = 0;
    uint32_t n = v < 0 ? -(uint32_t)v : (uint32_t)v;
    do
    {
        b[i++] = '0' + n % 10;
        n /= 10;
    }
    while(n);
    if(v < 0) putc_('-');
    while(i) putc_(b[--i]);
}

static void puthex(uint32_t v)
{
    for(int i = 28; i >= 0; i -= 4) putc_("0123456789ABCDEF"[v >> i & 15]);
}

// What's typed while a program runs (its Ctrl-C checks read it), for
// the next lines read: a pasted program, or one typed ahead
static char ahead[32];
static uint8_t ahead_in, ahead_out;

static int rx(void)
{
    return USART1_STATR & USART_RXNE ? (int)(USART1_DATAR & 0xff) : -1;
}

static int getc_wait(void)
{
    for(;;)
    {
        if(ahead_out != ahead_in) return ahead[ahead_out++ % sizeof ahead];
        int c = rx();
        if(c >= 0) return c;
        __asm__ volatile("wfi");
    }
}

void prompt(void) __attribute__((noreturn));
void report_fault(void) __attribute__((noreturn));

// Back to the prompt with a fresh stack, from anywhere
__attribute__((naked, noreturn)) static void to_prompt(void)
{
    __asm__ volatile("la sp, _estack\n j prompt");
}

// Where a fault returns to (trap_handler sets mepc to it)
__attribute__((naked, noreturn)) static void fault_entry(void)
{
    __asm__ volatile("la sp, _estack\n j report_fault");
}

__attribute__((interrupt("machine"), aligned(4)))
void trap_handler(void)
{
    uint32_t cause;
    __asm__ volatile("csrr %0, mcause" : "=r"(cause));
    if(cause & 0x80000000u)
    {
        if((cause & 0xff) == IRQ_SYSTICK)
        {
            STK_SR = 0;
            ms++;
        }
        return;
    }
    fault_cause = cause;
    __asm__ volatile("csrr %0, mepc" : "=r"(fault_pc));
    __asm__ volatile("csrr %0, mtval" : "=r"(fault_val));
    __asm__ volatile("csrw mepc, %0" ::"r"(fault_entry));
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
    // SysTick: HCLK / 8, an interrupt every ms
    STK_CTLR = 0;
    STK_SR = 0;
    STK_CNTL = 0;
    STK_CMPLR = HCLK_HZ / 8 / 1000;
    STK_CTLR = STK_STE | STK_STIE | STK_STRE;
    PFIC_IENR1 = 1u << IRQ_SYSTICK;
    __asm__ volatile("csrs mstatus, %0" ::"r"(0x88));
}

// ---- lines ----

static int line_num(int o) { return prog[o] | prog[o + 1] << 8; }
static int line_next(int o) { return o + prog[o + 2]; }
static const char *line_text(int o) { return (const char *)&prog[o + 3]; }

// The first line numbered n or more (prog_len if none)
static int find_line(int n)
{
    int o = 0;
    while(o < prog_len && line_num(o) < n) o = line_next(o);
    return o;
}

static uint8_t cancelled; // the last line read ended with Ctrl-C

// A line from the terminal, edited with backspace; Ctrl-C drops it.
// Letters outside quotes in capitals.
static void read_line(char *buf, int max)
{
    int n = 0;
    cancelled = 0;
    for(;;)
    {
        int c = getc_wait();
        if(c == '\r') break;
        if(c == 3)
        {
            puts_("^C");
            n = 0;
            cancelled = 1;
            break;
        }
        if((c == 8 || c == 127) && n)
        {
            n--;
            puts_("\b \b");
        }
        else if(c >= ' ' && c < 127 && n < max)
        {
            buf[n++] = c;
            putc_(c);
        }
    }
    buf[n] = 0;
    newline();
    int quoted = 0;
    for(char *p = buf; *p; p++)
    {
        if(*p == '"') quoted = !quoted;
        else if(!quoted && *p >= 'a' && *p <= 'z') *p -= 32;
    }
}

// ---- errors ----

__attribute__((noreturn)) static void error(const char *what)
{
    puts_("?");
    puts_(what);
    if(lp >= 0)
    {
        puts_(" IN ");
        putnum(line_num(lp));
    }
    newline();
    to_prompt();
}

// Ctrl-C typed while a program runs? What else was typed is kept for the
// next lines read, as far as there's room; more is lost, as the USART
// would overrun, so that a Ctrl-C always gets through
static void check_break(void)
{
    for(int c; (c = rx()) >= 0;)
    {
        if(c == 3)
        {
            ahead_in = ahead_out;
            error("BREAK");
        }
        if((uint8_t)(ahead_in - ahead_out) < sizeof ahead) ahead[ahead_in++ % sizeof ahead] = c;
    }
}

// ---- parsing ----

static void skip(void)
{
    while(*tp == ' ') tp++;
}

// The keyword k next (and past it)?
static int kw(const char *k)
{
    skip();
    const char *p = tp;
    while(*k)
    {
        if(*p++ != *k++) return 0;
    }
    tp = p;
    return 1;
}

static void expect(char c)
{
    skip();
    if(*tp != c) error("SYNTAX");
    tp++;
}

static int is_digit(char c) { return c >= '0' && c <= '9'; }
static int hex_digit(char c) { return is_digit(c) ? c - '0' : c >= 'A' && c <= 'F' ? c - 'A' + 10 : -1; }

static int32_t expr(void);

static int32_t number(void)
{
    uint32_t v = 0;
    if(*tp == '$')
    {
        tp++;
        if(hex_digit(*tp) < 0) error("SYNTAX");
        while(hex_digit(*tp) >= 0) v = v << 4 | hex_digit(*tp++);
    }
    else
    {
        while(is_digit(*tp)) v = v * 10 + (*tp++ - '0');
    }
    return (int32_t)v;
}

static int32_t paren(void)
{
    int32_t v = expr();
    expect(')');
    return v;
}

static int32_t primary(void)
{
    skip();
    char c = *tp;
    if(c == '(')
    {
        tp++;
        return paren();
    }
    if(is_digit(c) || c == '$') return number();
    if(kw("PEEK(")) return (int32_t)REG(paren());
    if(kw("RND("))
    {
        int32_t n = paren();
        // (the time stirred in: a program run after reset, typed or
        // pasted at a person's pace, gets different numbers each time)
        rng ^= ms;
        if(!rng) rng = 1;
        rng ^= rng << 13;
        rng ^= rng >> 17;
        rng ^= rng << 5;
        return n > 0 ? (int32_t)(rng % (uint32_t)n) + 1 : 0;
    }
    if(kw("ABS("))
    {
        int32_t v = paren();
        return v < 0 ? -v : v;
    }
    if(kw("TIME")) return (int32_t)ms;
    if(kw("FREE")) return PROG_SIZE - prog_len;
    if(c >= 'A' && c <= 'Z')
    {
        tp++;
        return var[c - 'A'];
    }
    error("SYNTAX");
}

static int32_t unary(void)
{
    skip();
    if(*tp == '-')
    {
        tp++;
        return -unary();
    }
    if(*tp == '+')
    {
        tp++;
        return unary();
    }
    if(kw("NOT")) return ~unary();
    return primary();
}

// Binary operators, by precedence (1 binds least): their text, in the order
// they're tried (<= before <)
static const struct { char text[4]; uint8_t prec; } ops[] = {
    {"OR", 1}, {"XOR", 1}, {"AND", 2},
    {"<=", 3}, {">=", 3}, {"<>", 3}, {"<", 3}, {">", 3}, {"=", 3},
    {"+", 4}, {"-", 4}, {"*", 5}, {"/", 5}, {"MOD", 5},
};

// An expression of operators binding at least as tightly as min
// (precedence climbing: a level of parentheses takes few stack frames)
static int32_t binary(int min)
{
    // The stack (the RAM above the variables) mustn't run into them, with
    // room left below for what's called from here and an interrupt
    extern char _ebss[];
    char *sp;
    __asm__("mv %0, sp" : "=r"(sp));
    if(sp < _ebss + 128) error("TOO COMPLEX");
    int32_t v = unary();
    for(;;)
    {
        const char *before = tp;
        unsigned op = 0;
        while(op < sizeof ops / sizeof ops[0] && !kw(ops[op].text)) op++;
        if(op == sizeof ops / sizeof ops[0] || ops[op].prec < min)
        {
            tp = before;
            return v;
        }
        int32_t r = binary(ops[op].prec + 1);
        switch(op)
        {
            case 0: v |= r; break;
            case 1: v ^= r; break;
            case 2: v &= r; break;
            case 3: v = -(v <= r); break;
            case 4: v = -(v >= r); break;
            case 5: v = -(v != r); break;
            case 6: v = -(v < r); break;
            case 7: v = -(v > r); break;
            case 8: v = -(v == r); break;
            case 9: v += r; break;
            case 10: v -= r; break;
            case 11: v *= r; break;
            default:
                if(!r) error("DIVISION BY ZERO");
                v = op == 12 ? v / r : v % r;
        }
    }
}

static int32_t expr(void) { return binary(1); }

static int variable(void)
{
    skip();
    char c = *tp;
    if(c < 'A' || c > 'Z') error("SYNTAX");
    tp++;
    return c - 'A';
}

// ---- the program ----

static void goto_line(int n)
{
    int o = find_line(n);
    if(o >= prog_len || line_num(o) != n) error("NO SUCH LINE");
    lp = o;
    tp = line_text(o);
    jumped = 1;
}

// Line n becomes text (or goes, if text is empty)
static void store_line(int n, const char *text)
{
    int o = find_line(n);
    if(o < prog_len && line_num(o) == n)
    {
        int len = prog[o + 2];
        for(int i = o; i + len < prog_len; i++) prog[i] = prog[i + len];
        prog_len -= len;
    }
    if(!*text) return;
    int tlen = 0;
    while(text[tlen]) tlen++;
    int len = 3 + tlen + 1;
    if(prog_len + len > PROG_SIZE) error("OUT OF MEMORY");
    for(int i = prog_len - 1; i >= o; i--) prog[i + len] = prog[i];
    prog[o] = n;
    prog[o + 1] = n >> 8;
    prog[o + 2] = len;
    for(int i = 0; i <= tlen; i++) prog[o + 3 + i] = text[i];
    prog_len += len;
}

static void list(void)
{
    for(int o = 0; o < prog_len; o = line_next(o))
    {
        putnum(line_num(o));
        putc_(' ');
        puts_(line_text(o));
        newline();
    }
}

static void flash_wait(void)
{
    while(FLASH_STATR & FLASH_BSY);
}

// The program into the last 1 KB of flash: erased, then written 64 bytes
// (a fast programming page) at a time
static void save(void)
{
    FLASH_KEYR = 0x45670123;
    FLASH_KEYR = 0xcdef89ab;
    FLASH_MODEKEYR = 0x45670123;
    FLASH_MODEKEYR = 0xcdef89ab;
    FLASH_CTLR = FLASH_PER;
    FLASH_ADDR = SAVE_ADDR;
    FLASH_CTLR = FLASH_PER | FLASH_STRT;
    flash_wait();
    int words = (8 + prog_len + 3) / 4;
    for(int page = 0; page < (words + 15) / 16; page++)
    {
        FLASH_CTLR = FLASH_FTPG;
        FLASH_CTLR = FLASH_FTPG | FLASH_BUFRST;
        flash_wait();
        for(int i = 0; i < 16; i++)
        {
            int w = page * 16 + i;
            uint32_t v = w == 0 ? SAVE_MAGIC : w == 1 ? (uint32_t)prog_len : 0;
            for(int b = 0; b < 4 && w >= 2; b++)
            {
                int at = (w - 2) * 4 + b;
                v |= (uint32_t)(at < prog_len ? prog[at] : 0xff) << (8 * b);
            }
            REG(SAVE_ADDR + 4 * w) = v;
            FLASH_CTLR = FLASH_FTPG | FLASH_BUFLOAD;
            flash_wait();
        }
        FLASH_ADDR = SAVE_ADDR + 64 * page;
        FLASH_CTLR = FLASH_FTPG | FLASH_STRT;
        flash_wait();
    }
    FLASH_CTLR = FLASH_LOCK | FLASH_FLOCK;
    putnum(prog_len);
    puts_(" BYTES SAVED");
    newline();
}

static int saved_len(void)
{
    uint32_t len = REG(SAVE_ADDR + 4);
    return REG(SAVE_ADDR) == SAVE_MAGIC && len <= PROG_SIZE ? (int)len : -1;
}

static void load(void)
{
    int len = saved_len();
    if(len < 0) error("NOTHING SAVED");
    const volatile uint8_t *p = (const volatile uint8_t *)(SAVE_ADDR + 8);
    for(int i = 0; i < len; i++) prog[i] = p[i];
    prog_len = len;
    putnum(len);
    puts_(" BYTES LOADED");
    newline();
}

// ---- statements ----

static void print(void)
{
    int nl = 1;
    for(;;)
    {
        skip();
        if(!*tp || *tp == ':') break;
        if(*tp == '"')
        {
            tp++;
            while(*tp && *tp != '"') putc_(*tp++);
            if(*tp) tp++;
        }
        else if(kw("HEX(")) puthex(paren());
        else putnum(expr());
        skip();
        nl = 1;
        if(*tp == ';') nl = 0;
        else if(*tp == ',') nl = 0, putc_(' ');
        else break;
        tp++;
    }
    if(nl) newline();
}

static void input(void)
{
    skip();
    if(*tp == '"')
    {
        tp++;
        while(*tp && *tp != '"') putc_(*tp++);
        expect('"');
        skip();
        if(*tp != ';' && *tp != ',') error("SYNTAX");
        tp++;
    }
    for(;;)
    {
        int v = variable();
        char buf[24];
        const char *here = tp;
        // a reply that isn't a whole expression is asked for again
        for(;;)
        {
            puts_("? ");
            read_line(buf, sizeof buf - 1);
            tp = buf;
            var[v] = expr();
            skip();
            if(!*tp) break;
            puts_("?REDO");
            newline();
        }
        tp = here;
        skip();
        if(*tp != ',') break;
        tp++;
    }
}

static void statement(void);

static void for_(void)
{
    int v = variable();
    expect('=');
    var[v] = expr();
    if(!kw("TO")) error("SYNTAX");
    int32_t limit = expr(), step = 1;
    if(kw("STEP")) step = expr();
    // a loop on the same variable again starts over
    for(int i = 0; i < nfor; i++)
    {
        if(fors[i].var == v)
        {
            nfor = i;
            break;
        }
    }
    if(nfor == FOR_DEPTH) error("TOO MANY FOR LOOPS");
    fors[nfor].var = v;
    fors[nfor].limit = limit;
    fors[nfor].step = step;
    fors[nfor].lp = lp;
    fors[nfor].tp = tp;
    nfor++;
}

static void next(void)
{
    skip();
    int i = nfor - 1;
    if(*tp >= 'A' && *tp <= 'Z')
    {
        int v = variable();
        while(i >= 0 && fors[i].var != v) i--;
    }
    if(i < 0) error("NEXT WITHOUT FOR");
    nfor = i + 1;
    int32_t x = var[fors[i].var] += fors[i].step;
    if(fors[i].step >= 0 ? x <= fors[i].limit : x >= fors[i].limit)
    {
        lp = fors[i].lp;
        tp = fors[i].tp;
    }
    else nfor = i;
}

static void statement(void)
{
    skip();
    if(kw("PRINT") || kw("?")) print();
    else if(kw("IF"))
    {
        int32_t c = expr();
        if(!kw("THEN")) error("SYNTAX");
        skip();
        if(!c)
        {
            while(*tp) tp++;
        }
        else if(is_digit(*tp)) goto_line(expr());
        else statement();
    }
    else if(kw("GOTO")) goto_line(expr());
    else if(kw("GOSUB"))
    {
        int n = expr();
        if(ngosub == GOSUB_DEPTH) error("TOO MANY GOSUBS");
        gosubs[ngosub].lp = lp;
        gosubs[ngosub].tp = tp;
        ngosub++;
        goto_line(n);
    }
    else if(kw("RETURN"))
    {
        if(!ngosub) error("RETURN WITHOUT GOSUB");
        ngosub--;
        lp = gosubs[ngosub].lp;
        tp = gosubs[ngosub].tp;
    }
    else if(kw("FOR")) for_();
    else if(kw("NEXT")) next();
    else if(kw("INPUT")) input();
    else if(kw("POKE"))
    {
        uint32_t a = expr();
        expect(',');
        REG(a) = expr();
    }
    else if(kw("DELAY"))
    {
        uint32_t start = ms, n = expr();
        while(ms - start < n)
        {
            check_break();
            __asm__ volatile("wfi");
        }
    }
    else if(kw("REM") || kw("'"))
    {
        while(*tp) tp++;
    }
    else if(kw("END") || kw("STOP"))
    {
        lp = -1;
        tp = "";
    }
    else if(kw("CLS")) puts_("\033[2J\033[H");
    else if(kw("RUN"))
    {
        for(int i = 0; i < 26; i++) var[i] = 0;
        nfor = ngosub = 0;
        if(!prog_len)
        {
            tp = "";
            return;
        }
        lp = 0;
        tp = line_text(0);
        jumped = 1;
    }
    else if(kw("LIST")) list();
    else if(kw("NEW")) prog_len = 0;
    else if(kw("SAVE")) save();
    else if(kw("LOAD")) load();
    else if(kw("MON"))
    {
        in_mon = 1;
        puts_("MONITOR: ADDR, ADDR.ADDR, ADDR: VALUES, ADDR R; Q FOR BASIC");
        newline();
        to_prompt();
    }
    else
    {
        if(kw("LET")) {}
        int v = variable();
        expect('=');
        var[v] = expr();
    }
}

// Runs from tp (a typed line, or a program line when lp >= 0) until the
// end of the line typed or of the program
static void run(void)
{
    for(;;)
    {
        // (a running program; a line typed in runs to its end, and what
        // follows it is read as lines)
        if(lp >= 0) check_break();
        statement();
        if(jumped)
        {
            jumped = 0;
            continue;
        }
        skip();
        if(*tp == ':')
        {
            tp++;
            continue;
        }
        if(*tp) error("SYNTAX");
        if(lp < 0) return;
        lp = line_next(lp);
        if(lp >= prog_len) return;
        tp = line_text(lp);
    }
}

// ---- the monitor ----

static void dump(uint32_t from, uint32_t to)
{
    for(uint32_t a = from & ~3u;; a += 4)
    {
        if(a == (from & ~3u) || !(a & 15))
        {
            if(a != (from & ~3u)) newline();
            puthex(a);
            putc_(':');
        }
        putc_(' ');
        puthex(REG(a));
        if(a >= (to & ~3u)) break;
    }
    newline();
}

static void monitor_line(void)
{
    putc_('*');
    read_line(inbuf, LINE_MAX);
    if(cancelled) in_mon = 0;
    const char *p = inbuf;
    enum { XAM, BLOCK, STORE } mode = XAM;
    uint32_t store = 0;
    for(;;)
    {
        while(*p == ' ') p++;
        if(!*p) return;
        if(*p == '.')
        {
            mode = BLOCK;
            p++;
        }
        else if(*p == ':')
        {
            mode = STORE;
            store = xam;
            p++;
        }
        else if(*p == 'R')
        {
            p++;
            ((void (*)(void))xam)();
        }
        else if(*p == 'Q')
        {
            in_mon = 0;
            return;
        }
        else if(hex_digit(*p) >= 0)
        {
            uint32_t v = 0;
            while(hex_digit(*p) >= 0) v = v << 4 | hex_digit(*p++);
            if(mode == STORE)
            {
                REG(store) = v;
                store += 4;
            }
            else if(mode == BLOCK)
            {
                dump(xam, v);
                xam = v & ~3u;
                mode = XAM;
            }
            else
            {
                xam = v & ~3u;
                // (not when a range or values follow)
                while(*p == ' ') p++;
                if(*p != '.' && *p != ':') dump(xam, xam);
            }
        }
        else
        {
            puts_("?");
            newline();
            return;
        }
    }
}

// ---- the prompt ----

static void basic_line(void)
{
    puts_("> ");
    read_line(inbuf, LINE_MAX);
    tp = inbuf;
    skip();
    if(is_digit(*tp))
    {
        int32_t n = number();
        if(n < 1 || n > 65535) error("LINE NUMBER");
        skip();
        store_line(n, tp);
        return;
    }
    run();
}

void prompt(void)
{
    for(;;)
    {
        lp = -1;
        jumped = 0;
        if(in_mon) monitor_line();
        else basic_line();
    }
}

void report_fault(void)
{
    static const char *const faults[] = {"INSTRUCTION ADDRESS", "INSTRUCTION ACCESS", "ILLEGAL INSTRUCTION", "BREAKPOINT",
        "LOAD ADDRESS", "LOAD ACCESS", "STORE ADDRESS", "STORE ACCESS"};
    puts_("?");
    if(fault_cause < 8) puts_(faults[fault_cause]);
    else putnum(fault_cause);
    puts_(" FAULT AT ");
    puthex(fault_pc);
    puts_(" (");
    puthex(fault_val);
    puts_(")");
    if(lp >= 0)
    {
        puts_(" IN ");
        putnum(line_num(lp));
    }
    newline();
    prompt();
}

int main(void)
{
    hw_init();
    puts_("\r\nTINY BASIC FOR THE CH32V003\r\n");
    putnum(PROG_SIZE);
    puts_(" BYTES FREE");
    if(saved_len() > 0) puts_(", A PROGRAM SAVED: LOAD");
    newline();
    prompt();
}
