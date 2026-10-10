/* A socketpair must be private to each branch, with its buffered data:
 * one end is used from the main process, the other end by a child forked
 * before the checkpoint (the common parent/child channel). */
#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>
#include "ffs.h"

static void fail(const char *msg) {
  printf("FAIL: %s (errno=%d)\n", msg, errno);
  ffs_assert(0, msg);
  fflush(stdout);
  abort();
}

int main(void) {
  int sv[2], local[2];
  if (socketpair(AF_UNIX, SOCK_STREAM, 0, sv) || socketpair(AF_UNIX, SOCK_STREAM, 0, local)) { perror("socketpair"); return 1; }
  pid_t pid = fork();
  if (pid < 0) { perror("fork"); return 1; }
  if (pid == 0) {
    close(sv[0]);
    char buf[16];
    ssize_t n = read(sv[1], buf, sizeof buf); /* blocked here at the checkpoint */
    if (n <= 0) _exit(90);
    buf[n] = 0;
    char reply[32];
    snprintf(reply, sizeof reply, "echo:%s", buf);
    if (write(sv[1], reply, strlen(reply)) < 0) _exit(91);
    _exit(0);
  }
  close(sv[1]);
  /* data buffered in the local pair before the checkpoint */
  if (write(local[0], "abc", 3) != 3) fail("write local");

  ffs_checkpoint("sockets");

  char buf[32];
  ssize_t n = read(local[1], buf, sizeof buf);
  if (n != 3 || memcmp(buf, "abc", 3) != 0) {
    printf("local read returned %zd\n", n);
    fail("buffered socket data not preserved");
  }
  if (write(sv[0], "hi", 2) != 2) fail("write to child");
  n = read(sv[0], buf, sizeof buf - 1);
  if (n <= 0) fail("no reply from child");
  buf[n] = 0;
  if (strcmp(buf, "echo:hi") != 0) {
    printf("reply '%s'\n", buf);
    fail("child reply wrong (socket shared with another branch?)");
  }
  int st;
  if (waitpid(pid, &st, 0) != pid || !WIFEXITED(st) || WEXITSTATUS(st) != 0) fail("child status");
  printf("OK\n");
  return 0;
}
