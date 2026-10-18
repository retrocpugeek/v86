// Reset entry: stack and gp, .data from flash, .bss cleared, the stack area
// filled with 0xa5, then main().
	.section .init, "ax"
	.globl _start
_start:
	.option push
	.option norelax
	la gp, __global_pointer$
	.option pop
	la sp, _estack
	la a0, _sidata
	la a1, _sdata
	la a2, _edata
1:	bgeu a1, a2, 2f
	lw t0, 0(a0)
	sw t0, 0(a1)
	addi a0, a0, 4
	addi a1, a1, 4
	j 1b
2:	la a0, _sbss
	la a1, _ebss
3:	bgeu a0, a1, 4f
	sw zero, 0(a0)
	addi a0, a0, 4
	j 3b
4:	// Fill the free RAM (the stack) with a pattern, so its peak use can be
	// seen: the lowest word that changed
	li t0, 0xa5a5a5a5
	la a1, _estack
5:	bgeu a0, a1, 6f
	sw t0, 0(a0)
	addi a0, a0, 4
	j 5b
6:	// INTSYSCR: no hardware register stacking or nesting (the trap
	// handler saves registers itself)
	csrw 0x804, zero
	la t0, trap_handler
	csrw mtvec, t0
	call main
7:	j 7b
