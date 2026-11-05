/* The desktop's animation player (player.c) */
#ifndef PLAYER_H_
#define PLAYER_H_

#include "contiki.h"

PROCESS_NAME(player_process);

/* Opens (or brings to the front) its window, playing */
void player_open(void);

#endif
