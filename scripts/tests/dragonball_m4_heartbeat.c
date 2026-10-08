/* Disposable guest liveness probe. Host timestamps, not guest clocks, measure
 * gaps. UDP loss invalidates a measurement; a gap includes scheduling/network. */
#include <arpa/inet.h>
#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <time.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc != 4 || strlen(argv[3]) != 32 || strspn(argv[3], "0123456789abcdef") != 32) return 2;
    char *end;
    long port = strtol(argv[2], &end, 10);
    if (*end || port < 1 || port > 65535) return 2;
    struct sockaddr_in destination = {.sin_family = AF_INET, .sin_port = htons((uint16_t)port)};
    if (inet_pton(AF_INET, argv[1], &destination.sin_addr) != 1) return 2;
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (fd < 0) return 3;
    /* Bound abandoned snapshot copies as well as normal probes. */
    for (unsigned long seq = 1; seq <= 120000; seq++) {
        char data[64];
        int size = snprintf(data, sizeof(data), "%s %lu", argv[3], seq);
        if (sendto(fd, data, (size_t)size, 0, (struct sockaddr *)&destination, sizeof(destination)) != size) return 4;
        struct timespec delay = {.tv_sec = 0, .tv_nsec = 5000000};
        while (nanosleep(&delay, &delay) && errno == EINTR) {}
    }
    close(fd);
    return 0;
}
