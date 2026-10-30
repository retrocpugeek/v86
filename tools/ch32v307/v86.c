/*
 * Additions for running this BSP on v86's emulated CH32V307 (also fine on
 * the real chip).
 */
#include <rtthread.h>

/* The idle thread sleeps until the next interrupt instead of spinning: on
 * the chip this saves power, in the emulator host CPU. */
static void idle_wfi(void)
{
    __asm__ volatile("wfi");
}

static int v86_idle_init(void)
{
    rt_thread_idle_sethook(idle_wfi);
    return 0;
}
INIT_APP_EXPORT(v86_idle_init);

/* Lua's os.rename: there's no file system */
#include <errno.h>
int rename(const char *from, const char *to)
{
    (void)from;
    (void)to;
    errno = ENOSYS;
    return -1;
}

/* The rest of the POSIX file calls picolibc's stdio (Lua's io library)
 * refers to: no file system, so they fail */
#include <sys/types.h>
int open(const char *path, int flags, ...) { (void)path; (void)flags; errno = ENOSYS; return -1; }
int close(int fd) { (void)fd; errno = ENOSYS; return -1; }
ssize_t read(int fd, void *buf, size_t n) { (void)fd; (void)buf; (void)n; errno = ENOSYS; return -1; }
ssize_t write(int fd, const void *buf, size_t n) { (void)fd; (void)buf; (void)n; errno = ENOSYS; return -1; }
off_t lseek(int fd, off_t off, int whence) { (void)fd; (void)off; (void)whence; errno = ENOSYS; return -1; }
int unlink(const char *path) { (void)path; errno = ENOSYS; return -1; }
