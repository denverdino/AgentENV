#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <unistd.h>

struct ram_cut {
    char marker[48];
    volatile uint64_t issued;
    volatile uint64_t completed;
    volatile uint64_t received;
    volatile uint64_t transmitted;
};

static uint64_t pattern(uint64_t sequence, unsigned index) {
    return sequence ^ ((uint64_t)index * UINT64_C(0x9e3779b97f4a7c15));
}

int main(int argc, char **argv) {
    if (argc != 3 || strlen(argv[2]) != 47) return 2;
    int memory = open("/dev/shm/db-m1-ram-cut", O_RDWR | O_CREAT, 0600);
    if (memory < 0 || ftruncate(memory, 4096) != 0) return 3;
    struct ram_cut *cut = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, memory, 0);
    if (cut == MAP_FAILED) return 4;
    if (strcmp(argv[1], "read") == 0) {
        if (memcmp(cut->marker, argv[2], 48) != 0) return 5;
        printf("%" PRIu64 " %" PRIu64 "\n", cut->issued, cut->completed);
        return 0;
    }
    if (strcmp(argv[1], "network-read") == 0) {
        if (memcmp(cut->marker, argv[2], 48) != 0) return 5;
        printf("%" PRIu64 " %" PRIu64 "\n", cut->received, cut->transmitted);
        return 0;
    }
    if (strcmp(argv[1], "echo") == 0) {
        if (memcmp(cut->marker, argv[2], 48) != 0) return 5;
        int socket_fd = socket(AF_INET, SOCK_DGRAM, 0);
        struct sockaddr_in address = { .sin_family = AF_INET, .sin_port = htons(50001),
            .sin_addr = { .s_addr = htonl(INADDR_ANY) } };
        if (socket_fd < 0 || bind(socket_fd, (struct sockaddr *)&address, sizeof(address)) != 0) return 10;
        int ready = open("/dev/shm/db-m1-echo-ready", O_WRONLY | O_CREAT, 0600);
        if (ready < 0 || close(ready) != 0) return 11;
        unsigned char packet[4096];
        for (;;) {
            struct sockaddr_in peer;
            socklen_t peer_size = sizeof(peer);
            ssize_t length = recvfrom(socket_fd, packet, sizeof(packet), 0,
                (struct sockaddr *)&peer, &peer_size);
            if (length < 0 && errno == EINTR) continue;
            if (length != (ssize_t)sizeof(packet)) return 12;
            ++cut->received;
            if (sendto(socket_fd, packet, length, 0, (struct sockaddr *)&peer, peer_size) != length) return 13;
            ++cut->transmitted;
        }
    }
    if (strcmp(argv[1], "write") != 0) return 6;
    int disk = open("/var/tmp/db-m1-cut-block", O_RDWR | O_CREAT | O_DIRECT, 0600);
    void *buffer = NULL;
    if (disk < 0 || posix_memalign(&buffer, 4096, 4096) != 0) return 7;
    uint64_t *words = buffer;
    for (unsigned i = 0; i < 512; ++i) words[i] = pattern(0, i);
    if (pwrite(disk, buffer, 4096, 0) != 4096 || fsync(disk) != 0) return 8;
    cut->issued = 0;
    cut->completed = 0;
    cut->received = 0;
    cut->transmitted = 0;
    memcpy(cut->marker, argv[2], 48);
    for (uint64_t n = 1; ; ++n) {
        for (unsigned i = 0; i < 512; ++i) words[i] = pattern(n, i);
        cut->issued = n;
        if (pwrite(disk, buffer, 4096, 0) != 4096 || fdatasync(disk) != 0) return 9;
        cut->completed = n;
    }
}
