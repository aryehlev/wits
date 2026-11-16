/* Two slow paths with known names and lines, for the `where` cases:
 *   where cpu    burns CPU in inner_loop, called from handle_request
 *   where disk   fsyncs in flush_log
 *   where N      CPU work of N million steps, then exits (wits run / diff)
 */
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

volatile unsigned long sink;

__attribute__((noinline)) void inner_loop(unsigned long n) {
    for (unsigned long i = 0; i < n; i++) sink += i * i;
}

__attribute__((noinline)) void handle_request(unsigned long n) { inner_loop(n); }

__attribute__((noinline)) void flush_log(int fd) {
    char b[4096];
    memset(b, 1, sizeof b);
    if (write(fd, b, sizeof b) < 0) _exit(1);
    fsync(fd);
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "cpu") == 0)
        for (;;) handle_request(1000000);
    if (argc > 2 && strcmp(argv[1], "disk") == 0) {
        int fd = open(argv[2], O_CREAT | O_WRONLY | O_TRUNC, 0644);
        for (;;) flush_log(fd);
    }
    if (argc > 1) handle_request(strtoul(argv[1], 0, 10) * 1000000UL);
    return 0;
}
