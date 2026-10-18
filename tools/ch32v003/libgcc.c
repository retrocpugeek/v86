// What GCC calls for RV32EC (no M extension) and freestanding code.
#include <stddef.h>
#include <stdint.h>

uint32_t __mulsi3(uint32_t a, uint32_t b)
{
    uint32_t r = 0;
    while(b)
    {
        if(b & 1) r += a;
        a <<= 1;
        b >>= 1;
    }
    return r;
}

static uint32_t udivmod(uint32_t n, uint32_t d, uint32_t *rem)
{
    uint32_t q = 0, r = 0;
    for(int i = 31; i >= 0; i--)
    {
        r = r << 1 | (n >> i & 1);
        if(r >= d)
        {
            r -= d;
            q |= 1u << i;
        }
    }
    *rem = r;
    return q;
}

uint32_t __udivsi3(uint32_t n, uint32_t d) { uint32_t r; return udivmod(n, d, &r); }
uint32_t __umodsi3(uint32_t n, uint32_t d) { uint32_t r; udivmod(n, d, &r); return r; }

int32_t __divsi3(int32_t n, int32_t d)
{
    uint32_t r;
    uint32_t q = udivmod(n < 0 ? -(uint32_t)n : (uint32_t)n, d < 0 ? -(uint32_t)d : (uint32_t)d, &r);
    return (n < 0) != (d < 0) ? -(int32_t)q : (int32_t)q;
}

int32_t __modsi3(int32_t n, int32_t d)
{
    uint32_t r;
    udivmod(n < 0 ? -(uint32_t)n : (uint32_t)n, d < 0 ? -(uint32_t)d : (uint32_t)d, &r);
    return n < 0 ? -(int32_t)r : (int32_t)r;
}

void *memset(void *p, int c, size_t n)
{
    uint8_t *b = p;
    while(n--) *b++ = c;
    return p;
}

void *memcpy(void *d, const void *s, size_t n)
{
    uint8_t *a = d;
    const uint8_t *b = s;
    while(n--) *a++ = *b++;
    return d;
}
