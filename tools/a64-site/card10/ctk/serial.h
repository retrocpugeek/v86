/* The desktop's serial terminal (serial.c) */
#ifndef SERIAL_H_
#define SERIAL_H_

#include "contiki.h"

PROCESS_NAME(serial_process);

/* Opens (or brings to the front) its window */
void serial_open(void);

#endif
