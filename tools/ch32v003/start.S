// Reset entry: stack and gp, .data from flash, .bss cleared, then main().
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
4:	// INTSYSCR: no hardware register stacking or nesting (the trap
	// handler saves registers itself)
	csrw 0x804, zero
	la t0, trap_handler
	csrw mtvec, t0
	call main
5:	j 5b
