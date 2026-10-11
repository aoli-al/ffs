#include <assert.h>
#include <errno.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static long long now(void) {
    struct timespec t;
    assert(clock_gettime(CLOCK_MONOTONIC, &t) == 0);
    return t.tv_sec * 1000000000LL + t.tv_nsec;
}

static void waits(void) {
    int ep = epoll_create1(EPOLL_CLOEXEC);
    assert(ep >= 0);
    long page = sysconf(_SC_PAGESIZE);
    void *buf = mmap(NULL, page * 2, PROT_READ | PROT_WRITE,
                     MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    assert(buf != MAP_FAILED);
    assert(munmap((char *)buf + page, page) == 0);
    long long start = now();
    assert(epoll_wait(ep, buf, page * 2 / sizeof(struct epoll_event), 20) == 0);
    assert(now() - start >= 20000000);

    struct timespec timeout = {0, 20000000};
    start = now();
    assert(syscall(SYS_ppoll, NULL, 0, &timeout, NULL, 8) == 0);
    assert(now() - start >= 20000000);
    assert(timeout.tv_sec == 0 && timeout.tv_nsec == 0);

    timeout = (struct timespec){0, 20000000};
    start = now();
    assert(syscall(SYS_pselect6, 0, NULL, NULL, NULL, &timeout, NULL) == 0);
    assert(now() - start >= 20000000);
    assert(timeout.tv_sec == 0 && timeout.tv_nsec == 0);

    // The preemption signal must be removed from the signal mask, not
    // from the timeout argument. Both caller buffers must be preserved.
    timeout = (struct timespec){0, 20000000};
    uint64_t mask = UINT64_MAX;
    start = now();
    assert(syscall(SYS_epoll_pwait2, ep, buf, 1, &timeout, &mask, 8) == 0);
    assert(now() - start >= 20000000);
    assert(timeout.tv_sec == 0 && timeout.tv_nsec == 20000000);
    assert(mask == UINT64_MAX);

    // Exercise an internal preemption signal while a non-restartable wait
    // is in the kernel. It must not escape to the application as EINTR.
    pid_t child = fork();
    assert(child >= 0);
    if (!child) {
        struct timespec delay = {0, 5000000};
        assert(nanosleep(&delay, NULL) == 0);
        assert(kill(getppid(), SIGSTKFLT) == 0);
        delay.tv_nsec = 100000000;
        assert(nanosleep(&delay, NULL) == 0);
        _exit(0);
    }
    start = now();
    assert(epoll_wait(ep, buf, 1, 20) == 0);
    assert(now() - start >= 20000000);
    int status;
    assert(waitpid(child, &status, 0) == child && status == 0);
    close(ep);
    munmap(buf, page);
}

static void clone_parent(void) {
    pid_t original = getpid();
    pid_t child = fork();
    assert(child >= 0);
    if (!child) {
        long sibling = syscall(SYS_clone, CLONE_PARENT | SIGCHLD, 0, 0, 0, 0);
        assert(sibling >= 0);
        if (!sibling) {
            assert(getppid() == original);
            _exit(77);
        }
        _exit(76);
    }
    int sum = 0, status;
    for (int i = 0; i < 2; ++i) {
        assert(wait(&status) > 0 && WIFEXITED(status));
        sum += WEXITSTATUS(status);
    }
    assert(sum == 153);
    assert(wait(&status) == -1 && errno == ECHILD);
}

static void tsc(void) {
    int mode;
    assert(prctl(PR_GET_TSC, &mode) == 0 && mode == PR_TSC_ENABLE);
    assert(prctl(PR_SET_TSC, PR_TSC_SIGSEGV) == 0);
    assert(prctl(PR_GET_TSC, &mode) == 0 && mode == PR_TSC_SIGSEGV);
    pid_t child = fork();
    assert(child >= 0);
    if (!child) {
        __asm__ volatile("rdtsc" ::: "rax", "rdx");
        _exit(1);
    }
    int status;
    assert(waitpid(child, &status, 0) == child);
    assert(WIFSIGNALED(status) && WTERMSIG(status) == SIGSEGV);
    assert(prctl(PR_SET_TSC, PR_TSC_ENABLE) == 0);
    assert(prctl(PR_GET_TSC, &mode) == 0 && mode == PR_TSC_ENABLE);
    unsigned low, high;
    __asm__ volatile("rdtsc" : "=a"(low), "=d"(high));
    char text[80];
    int len = snprintf(text, sizeof(text), "tsc=%llu\n", ((unsigned long long)high << 32) | low);
    assert(write(1, text, len) == len);
    assert(prctl(PR_SET_TSC, 99) == -1 && errno == EINVAL);
    assert(prctl(PR_GET_TSC, &mode) == 0 && mode == PR_TSC_ENABLE);
}

int main(int argc, char **argv) {
    assert(argc == 2);
    if (!strcmp(argv[1], "waits")) waits();
    else if (!strcmp(argv[1], "clone_parent")) clone_parent();
    else if (!strcmp(argv[1], "tsc")) tsc();
    else if (!strcmp(argv[1], "cpu")) assert(sysconf(_SC_NPROCESSORS_ONLN) == 1);
    else if (!strcmp(argv[1], "vm_write")) {
        char src[] = "copied", dst[sizeof(src)] = {0};
        struct iovec in = {src, sizeof(src)}, out = {dst, sizeof(dst)};
        assert(process_vm_writev(getpid(), &in, 1, &out, 1, 0) == sizeof(src));
        assert(memcmp(src, dst, sizeof(src)) == 0);
    } else assert(0);
    assert(write(1, "EXIT-SUCCESS\n", 13) == 13);
}
