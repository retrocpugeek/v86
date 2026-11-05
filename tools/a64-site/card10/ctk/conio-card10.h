/* The conio interface CTK's drawing (ctk-conio.c) uses, on the card10's
   display: conio-card10.c. */
#ifndef CONIO_CARD10_H_
#define CONIO_CARD10_H_

#define CONIO_WIDTH   40
#define CONIO_HEIGHT  13

unsigned char wherex(void);
unsigned char wherey(void);
void clrscr(void);
unsigned char bgcolor(unsigned char c);
unsigned char bordercolor(unsigned char c);
unsigned char textcolor(unsigned char c);
void screensize(unsigned char *x, unsigned char *y);
unsigned char revers(unsigned char c);
void cputc(char c);
void cputs(const char *str);
void cclear(unsigned char length);
void chline(unsigned char length);
void cvline(unsigned char length);
void gotoxy(unsigned char x, unsigned char y);
void cclearxy(unsigned char x, unsigned char y, unsigned char length);
void chlinexy(unsigned char x, unsigned char y, unsigned char length);
void cvlinexy(unsigned char x, unsigned char y, unsigned char length);
void cputsxy(unsigned char x, unsigned char y, const char *str);
void cputcxy(unsigned char x, unsigned char y, char c);

/* Draws what changed into the display; returns whether anything did */
int conio_flush(void);

#endif /* CONIO_CARD10_H_ */
