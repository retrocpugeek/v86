// memset and memcpy for the games, which link without a C library: GCC 14
// emits calls to them (e.g. for zeroing arrays) that older versions inlined.
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
