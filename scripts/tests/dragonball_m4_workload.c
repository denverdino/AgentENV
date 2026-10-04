/* Static guest helper: nonzero anonymous RAM with bounded repeated writes.
 * A fresh request verifies every byte of actual RAM, including after restore.
 */
#include <errno.h>
#include <inttypes.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

static double seconds(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

int main(int argc, char **argv) {
    if (argc != 5) return 2;
    uint64_t size = strtoull(argv[1], NULL, 10);
    uint64_t dirty = strtoull(argv[2], NULL, 10);
    unsigned long interval = strtoul(argv[3], NULL, 10);
    if (!size || size > 4ULL * 1024 * 1024 * 1024 || !dirty || dirty > size ||
        interval < 10 || interval > 10000 || chdir(argv[4])) return 2;
    volatile unsigned char *ram = mmap(NULL, size, PROT_READ | PROT_WRITE,
                                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (ram == MAP_FAILED) return 3;
    for (uint64_t i = 0; i < size; i++) ram[i] = (unsigned char)(1 + i % 251);
    FILE *pid = fopen("pid", "w");
    if (!pid) return 4;
    fprintf(pid, "%ld\n", (long)getpid());
    fclose(pid);
    uint64_t written = 0;
    char previous[128] = "";
    double start = seconds();
    while (seconds() - start < 7200) {
        for (uint64_t i = 0; i < dirty; i++) {
            unsigned char expected = (unsigned char)(1 + i % 251);
            if (ram[i] != expected) {
                fprintf(stderr, "anonymous RAM corruption before dirty write at byte %" PRIu64 "\n", i);
                return 5;
            }
            ram[i] = expected;
        }
        written += dirty;
        FILE *request = fopen("request", "r");
        char token[128] = "";
        if (request) {
            if (fscanf(request, "%127[a-zA-Z0-9_-]", token) != 1) token[0] = 0;
            fclose(request);
        }
        if (token[0] && strcmp(token, previous)) {
            int valid = 1;
            uint64_t checksum = 0;
            for (uint64_t i = 0; i < size; i++) {
                unsigned char value = ram[i];
                checksum += value;
                if (value != (unsigned char)(1 + i % 251)) valid = 0;
            }
            FILE *response = fopen("response.tmp", "w");
            if (!response) return 4;
            fprintf(response, "{\"token\":\"%s\",\"valid\":%s,\"resident_bytes\":%" PRIu64
                    ",\"dirty_bytes_per_sweep\":%" PRIu64 ",\"written_bytes\":%" PRIu64
                    ",\"checksum\":%" PRIu64 ",\"elapsed_seconds\":%.6f}\n",
                    token, valid ? "true" : "false", size, dirty, written, checksum, seconds() - start);
            if (fclose(response) || rename("response.tmp", "response")) return 4;
            strcpy(previous, token);
            if (!valid) return 5;
        }
        struct timespec pause = {(time_t)(interval / 1000), (long)(interval % 1000) * 1000000};
        while (nanosleep(&pause, &pause) && errno == EINTR) {}
    }
    munmap((void *)ram, size);
    return 0;
}
