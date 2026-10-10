/* Kernel-side fd state must be preserved: bytes buffered in a pipe and the
 * counter of an eventfd, both created before the checkpoint, must be
 * readable exactly once in the branch and the pipe must keep working. */
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <unistd.h>
#include "ffs.h"

static void fail(const char *msg) {
  printf("FAIL: %s (errno=%d)\n", msg, errno);
  ffs_assert(0, msg);
  fflush(stdout);
  abort();
}

int main(void) {
  int fds[2];
  if (pipe2(fds, O_NONBLOCK) != 0) { perror("pipe2"); return 1; }
  if (write(fds[1], "abc", 3) != 3) { perror("write"); return 1; }
  int efd = eventfd(2, EFD_NONBLOCK);
  if (efd < 0) { perror("eventfd"); return 1; }

  ffs_checkpoint("fds");

  char buf[16];
  ssize_t n = read(fds[0], buf, sizeof buf);
  if (n != 3 || memcmp(buf, "abc", 3) != 0) {
    printf("pipe read returned %zd\n", n);
    fail("pipe contents not preserved");
  }
  n = read(fds[0], buf, sizeof buf);
  if (n != -1 || errno != EAGAIN) fail("pipe not empty after draining 3 bytes");

  uint64_t v = 0;
  n = read(efd, &v, sizeof v);
  if (n != (ssize_t)sizeof v || v != 2) {
    printf("eventfd read returned %zd value %llu\n", n, (unsigned long long)v);
    fail("eventfd counter not preserved");
  }
  n = read(efd, &v, sizeof v);
  if (n != -1 || errno != EAGAIN) fail("eventfd not zero after being read");

  if (write(fds[1], "xyz", 3) != 3) fail("pipe write after checkpoint failed");
  n = read(fds[0], buf, sizeof buf);
  if (n != 3 || memcmp(buf, "xyz", 3) != 0)
    fail("pipe round trip after checkpoint failed");
  printf("OK\n");
  return 0;
}
