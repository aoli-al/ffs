// A child that has exited but not been waited for keeps its pid: kill()
// and pidfd_open() on it succeed, the pidfd polls readable, and only the
// wait makes the pid vanish.  Under ffs the tracer reaps every child at
// once, so these must be emulated from the virtual process tree.
#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

int main(void) {
    pid_t pid = fork();
    if (pid == 0) {
        _exit(3);
    }
    struct timespec ts = {0, 10 * 1000 * 1000};
    nanosleep(&ts, NULL); // the child is long gone
    if (kill(pid, 0) != 0) {
        perror("kill(zombie, 0)");
        abort();
    }
    if (kill(pid, SIGTERM) != 0) {
        perror("kill(zombie, SIGTERM)");
        abort();
    }
    int pidfd = (int)syscall(SYS_pidfd_open, pid, 0);
    if (pidfd < 0) {
        perror("pidfd_open(zombie)");
        abort();
    }
    struct pollfd pfd = {.fd = pidfd, .events = POLLIN};
    if (poll(&pfd, 1, 0) != 1 || !(pfd.revents & POLLIN)) {
        fprintf(stderr, "pidfd of a zombie is not readable (revents %#x)\n", pfd.revents);
        abort();
    }
    int status = 0;
    if (waitpid(pid, &status, WNOHANG) != pid || !WIFEXITED(status) || WEXITSTATUS(status) != 3) {
        fprintf(stderr, "waitpid: status %#x\n", status);
        abort();
    }
    if (kill(pid, 0) == 0 || errno != ESRCH) {
        fprintf(stderr, "reaped pid still signalable\n");
        abort();
    }
    close(pidfd);
    printf("ok\n");
    return 0;
}
