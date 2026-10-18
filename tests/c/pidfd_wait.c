// A child's pidfd becoming readable means the child is a zombie the parent
// can reap at once: waitpid(WNOHANG) right after poll() must return it.
// tokio's process reaper relies on this (pidfd_reaper: poll, then
// try_wait).  Under ffs the wait is emulated from the tracer's own
// bookkeeping, which must catch up with the kernel first.
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void) {
    for (int round = 0; round < 20; round++) {
        pid_t pid = fork();
        if (pid == 0) {
            _exit(7);
        }
        int pidfd = (int)syscall(SYS_pidfd_open, pid, 0);
        if (pidfd < 0) {
            perror("pidfd_open");
            return 2;
        }
        struct pollfd pfd = {.fd = pidfd, .events = POLLIN};
        if (poll(&pfd, 1, -1) != 1) {
            perror("poll");
            return 2;
        }
        int status = 0;
        pid_t r = waitpid(pid, &status, WNOHANG);
        if (r != pid) {
            fprintf(stderr, "round %d: pidfd readable but waitpid(WNOHANG) returned %d\n", round, (int)r);
            abort();
        }
        if (!WIFEXITED(status) || WEXITSTATUS(status) != 7) {
            fprintf(stderr, "round %d: status %#x\n", round, status);
            abort();
        }
        close(pidfd);
    }
    printf("ok\n");
    return 0;
}
