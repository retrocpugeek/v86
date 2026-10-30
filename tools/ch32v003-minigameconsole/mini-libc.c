// The C library functions the console's firmware uses, for linking it without
// one (MounRiver links newlib-nano; this build has none): memset and memcpy,
// which GCC also emits calls to, strlen, and newlib's itoa.
// "used": with -flto the calls only appear after these would be dropped.
#include <stddef.h>

__attribute__((used)) void *memset(void *d, int c, size_t n)
{
    unsigned char *p = d;
    while(n--) *p++ = (unsigned char)c;
    return d;
}

__attribute__((used)) void *memcpy(void *d, const void *s, size_t n)
{
    unsigned char *p = d;
    const unsigned char *q = s;
    while(n--) *p++ = *q++;
    return d;
}

__attribute__((used)) size_t strlen(const char *s)
{
    const char *p = s;
    while(*p) p++;
    return p - s;
}

// newlib's: a minus sign only in base 10
__attribute__((used)) char *itoa(int value, char *str, int base)
{
    char *p = str;
    unsigned int u = value;
    if(base < 2 || base > 36) { *str = 0; return str; }
    if(base == 10 && value < 0) { *p++ = '-'; u = -(unsigned int)value; }
    char *start = p;
    do { unsigned int d = u % base; *p++ = d < 10 ? '0' + d : 'a' + d - 10; u /= base; } while(u);
    *p = 0;
    for(char *e = p - 1; start < e; start++, e--) { char t = *start; *start = *e; *e = t; }
    return str;
}
