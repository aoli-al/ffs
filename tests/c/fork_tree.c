/* The process tree must survive a checkpoint: a child forked before the
 * checkpoint is still our child in every branch (waitpid works, with the
 * right status), it still sees us as its parent, and we get SIGCHLD with
 * the right siginfo when it exits. */
#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>
#include "ffs.h"

static volatile pid_t chld_pid;
static volatile int chld_status, chld_code;

static void fail(const char *msg) {
  printf("FAIL: %s (errno=%d)\n", msg, errno);
  ffs_assert(0, msg);
  fflush(stdout);
  abort();
}

static void on_chld(int sig, siginfo_t *si, void *ctx) {
  (void)sig; (void)ctx;
  chld_pid = si->si_pid;
  chld_status = si->si_status;
  chld_code = si->si_code;
}

int main(void) {
  int to_child[2], from_child[2];
  if (pipe(to_child) || pipe(from_child)) { perror("pipe"); return 1; }
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_sigaction = on_chld;
  sa.sa_flags = SA_SIGINFO | SA_RESTART;
  sigaction(SIGCHLD, &sa, NULL);

  pid_t pid = fork();
  if (pid < 0) { perror("fork"); return 1; }
  if (pid == 0) {
    pid_t pp = getppid();
    if (write(from_child[1], &pp, sizeof pp) != sizeof pp) _exit(98);
    char c;
    if (read(to_child[0], &c, 1) != 1) _exit(97); /* blocked here at the checkpoint */
    if (getppid() != pp) _exit(99);
    _exit(7);
  }
  pid_t reported = 0;
  if (read(from_child[0], &reported, sizeof reported) != sizeof reported) fail("read from child");
  if (reported != getpid()) fail("child's getppid() is not our pid");

  ffs_checkpoint("tree");

  int st = -1;
  if (waitpid(pid, &st, WNOHANG) != 0) fail("waitpid(WNOHANG) did not return 0 for a live child");
  if (write(to_child[1], "g", 1) != 1) fail("write to child");
  pid_t w = waitpid(pid, &st, 0);
  if (w != pid) fail("waitpid returned the wrong pid");
  if (!WIFEXITED(st)) fail("child did not exit normally");
  if (WEXITSTATUS(st) != 7) {
    printf("exit status %d\n", WEXITSTATUS(st));
    fail("child reported the wrong exit status (99 = getppid changed)");
  }
  if (chld_pid != pid || chld_status != 7 || chld_code != CLD_EXITED) {
    printf("SIGCHLD si_pid=%d si_status=%d si_code=%d (want %d 7 %d)\n", chld_pid, chld_status, chld_code, pid, CLD_EXITED);
    fail("SIGCHLD siginfo wrong");
  }
  if (waitpid(-1, &st, 0) != -1 || errno != ECHILD) fail("expected ECHILD with no children left");
  printf("OK\n");
  return 0;
}
