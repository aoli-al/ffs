// A minimal io_uring user (raw syscalls, no liburing): one ring, a READ of
// this file submitted by io_uring_enter, result read from the completion
// queue.  Under ffs the ring is a synchronous facade: when enter returns,
// the completion is there.
#include <fcntl.h>
#include <linux/io_uring.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

int main(int argc, char **argv) {
    const char *path = argc > 1 ? argv[1] : argv[0];
    struct io_uring_params p;
    memset(&p, 0, sizeof p);
    int ring = (int)syscall(__NR_io_uring_setup, 4, &p);
    if (ring < 0) {
        perror("io_uring_setup");
        return 2;
    }
    size_t sq_sz = p.sq_off.array + p.sq_entries * sizeof(unsigned);
    size_t cq_sz = p.cq_off.cqes + p.cq_entries * sizeof(struct io_uring_cqe);
    unsigned char *sq = mmap(NULL, sq_sz, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_POPULATE, ring, IORING_OFF_SQ_RING);
    unsigned char *cq = mmap(NULL, cq_sz, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_POPULATE, ring, IORING_OFF_CQ_RING);
    struct io_uring_sqe *sqes = mmap(NULL, p.sq_entries * sizeof(struct io_uring_sqe), PROT_READ | PROT_WRITE,
                                     MAP_SHARED | MAP_POPULATE, ring, IORING_OFF_SQES);
    if (sq == MAP_FAILED || cq == MAP_FAILED || sqes == MAP_FAILED) {
        perror("mmap");
        return 2;
    }
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        perror("open");
        return 2;
    }
    static char buf[4096];
    for (int round = 0; round < 3; round++) {
        unsigned tail = *(unsigned *)(sq + p.sq_off.tail);
        unsigned idx = tail & *(unsigned *)(sq + p.sq_off.ring_mask);
        struct io_uring_sqe *sqe = &sqes[idx];
        memset(sqe, 0, sizeof *sqe);
        sqe->opcode = IORING_OP_READ;
        sqe->fd = fd;
        sqe->addr = (unsigned long)buf;
        sqe->len = sizeof buf;
        sqe->off = 0;
        sqe->user_data = 100 + round;
        ((unsigned *)(sq + p.sq_off.array))[idx] = idx;
        atomic_store_explicit((_Atomic unsigned *)(sq + p.sq_off.tail), tail + 1, memory_order_release);
        long r = syscall(__NR_io_uring_enter, ring, 1, 0, 0, NULL, 0); // min_complete 0: natively may return before completion
        if (r < 0) {
            perror("io_uring_enter");
            return 2;
        }
        // Wait for the completion (natively a second enter with GETEVENTS).
        unsigned head = *(unsigned *)(cq + p.cq_off.head);
        while (atomic_load_explicit((_Atomic unsigned *)(cq + p.cq_off.tail), memory_order_acquire) == head) {
            syscall(__NR_io_uring_enter, ring, 0, 1, IORING_ENTER_GETEVENTS, NULL, 0);
        }
        struct io_uring_cqe *cqe = &((struct io_uring_cqe *)(cq + p.cq_off.cqes))[head & *(unsigned *)(cq + p.cq_off.ring_mask)];
        if (cqe->user_data != 100u + round || cqe->res <= 0) {
            fprintf(stderr, "round %d: cqe user_data=%llu res=%d\n", round, (unsigned long long)cqe->user_data, cqe->res);
            abort();
        }
        atomic_store_explicit((_Atomic unsigned *)(cq + p.cq_off.head), head + 1, memory_order_release);
    }
    printf("ok\n");
    return 0;
}
