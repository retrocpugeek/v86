// The few CH32V003 registers the firmware uses (CH32V003 reference manual).
#pragma once
#include <stdint.h>

#define REG(a) (*(volatile uint32_t *)(a))

// RCC
#define RCC_CTLR      REG(0x40021000)
#define RCC_CFGR0     REG(0x40021004)
#define RCC_APB2PCENR REG(0x40021018)
#define RCC_AFIOEN    (1u << 0)
#define RCC_IOPDEN    (1u << 5)
#define RCC_USART1EN  (1u << 14)

// GPIO port D
#define GPIOD_CFGLR   REG(0x40011400)

// USART1 (TX on PD5, RX on PD6)
#define USART1_STATR  REG(0x40013800)
#define USART1_DATAR  REG(0x40013804)
#define USART1_BRR    REG(0x40013808)
#define USART1_CTLR1  REG(0x4001380c)
#define USART_TXE     (1u << 7)
#define USART_RXNE    (1u << 5)
#define USART_UE      (1u << 13)
#define USART_TE      (1u << 3)
#define USART_RE      (1u << 2)

// SysTick
#define STK_CTLR      REG(0xe000f000)
#define STK_SR        REG(0xe000f004)
#define STK_CNTL      REG(0xe000f008)
#define STK_CMPLR     REG(0xe000f010)
#define STK_STE       (1u << 0)
#define STK_STIE      (1u << 1)
#define STK_STRE      (1u << 3)

// PFIC
#define PFIC_IENR1    REG(0xe000e100)
#define IRQ_SYSTICK   12

#define HCLK_HZ       24000000u
