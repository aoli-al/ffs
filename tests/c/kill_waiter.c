// SIGKILL of a process blocked in wait4 (an emulated wait under ffs) while
// its grandchild sleeps: the killer then reaps it.  tokio's kill_on_drop
// test does this to a `bash -c 'sh -c "sleep 1" & sleep 5'`.
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

int main(void) {
    pid_t mid = fork();
    if (mid == 0) {
        pid_t leaf = fork();
        if (leaf == 0) {
            struct timespec ts = {5, 0};
            nanosleep(&ts, NULL);
            _exit(0);
        }
        int st;
        waitpid(leaf, &st, 0);
        _exit(1);
    }
    struct timespec ts = {0, 20 * 1000 * 1000};
    nanosleep(&ts, NULL);
    if (kill(mid, SIGKILL) != 0) {
        perror("kill");
        abort();
    }
    int st = 0;
    if (waitpid(mid, &st, 0) != mid) {
        perror("waitpid");
        abort();
    }
    if (!WIFSIGNALED(st) || WTERMSIG(st) != SIGKILL) {
        fprintf(stderr, "status %#x\n", st);
        abort();
    }
    printf("ok\n");
    return 0;
}
