/* Contiki's real-time timers aren't used on the card10: the type only. */
#ifndef RTIMER_ARCH_H_
#define RTIMER_ARCH_H_
#define RTIMER_ARCH_SECOND 1000
rtimer_clock_t rtimer_arch_now(void);
#endif
