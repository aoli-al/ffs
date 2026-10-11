#include <assert.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <unistd.h>
#include <errno.h>
#include <time.h>

int main(int argc, char **argv) {
    assert(argc >= 2);
    if (!strcmp(argv[1], "virtual")) {
        struct timespec before = {0}, after = {0}, delay = {0, 1000000};
        long pid = syscall(SYS_getpid), tid = syscall(SYS_gettid);
        assert(syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &before) == 0);
        assert(syscall(SYS_nanosleep, &delay, NULL) == 0);
        assert(syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &after) == 0);
        char text[160];
        int n = snprintf(text, sizeof(text), "%ld %ld %ld.%09ld %ld.%09ld\n",
                         pid, tid, before.tv_sec, before.tv_nsec, after.tv_sec, after.tv_nsec);
        assert(write(1, text, n) == n);
        return 0;
    }
    if (!strcmp(argv[1], "compat")) {
        long result;
        __asm__ volatile("int $0x80" : "=a"(result) : "a"(20) : "memory");
        return result <= 0;
    }
    if (!strcmp(argv[1], "async")) {
        const long calls[] = {206, 209, 298, 323, 425, 426, 427};
        for (unsigned i = 0; i < sizeof(calls) / sizeof(calls[0]); ++i) {
            errno = 0;
            assert(syscall(calls[i], 0, 0, 0, 0, 0, 0) == -1);
            assert(errno == ENOSYS);
        }
        puts("refused async interfaces");
        return 0;
    }
    if (!strcmp(argv[1], "timerfd")) {
        errno = 0;
        assert(syscall(SYS_timerfd_create, 1, 0) == -1);
        assert(errno == ENOSYS);
        return 0;
    }
    if (!strcmp(argv[1], "pipe")) {
        int fds[2];
        assert(pipe(fds) == 0);
        assert(write(fds[1], "internal", 8) == 8);
        char buf[8];
        assert(read(fds[0], buf, 8) == 8);
        assert(write(1, buf, 8) == 8);
        return 0;
    }
    assert(argc == 3);
    if (!strcmp(argv[1], "stat")) {
        struct stat st;
        memset(&st, 0, sizeof(st));
        assert(syscall(SYS_newfstatat, AT_FDCWD, argv[2], &st, 0) == 0);
        char text[64];
        int n = snprintf(text, sizeof(text), "size=%ld\n", st.st_size);
        assert(write(1, text, n) == n);
        return 0;
    }
    int fd = open(argv[2], O_RDONLY);
    assert(fd >= 0);
    char buf[8] = {0};
    long n;
    if (!strcmp(argv[1], "readv")) {
        struct iovec iov[] = {{buf, 2}, {buf + 2, 6}};
        n = readv(fd, iov, 2);
    } else {
        n = read(fd, buf, sizeof(buf));
    }
    assert(n >= 0);
    // Equal lengths and identical control flow even when the bytes change.
    assert(write(STDOUT_FILENO, buf, n) == n);
    if (!strcmp(argv[1], "offset")) {
        assert(lseek(fd, 0, SEEK_CUR) == n);
        assert(read(fd, buf, 1) == 0);
    }
    close(fd);
    return 0;
}
