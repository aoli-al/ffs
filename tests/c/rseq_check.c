/* glibc registers an rseq area for every thread at start-up.  In a faithful
 * copy that registration is still in place for main and for a worker that
 * was blocked across the checkpoint, so re-registering must be refused.
 * The kernel answers EBUSY for the same area and EINVAL for a different one
 * (as here); ENOSYS means the kernel has no rseq at all.  0 means the thread
 * lost its registration. */
#include <errno.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/syscall.h>
#include <unistd.h>
#include "ffs.h"

#ifndef SYS_rseq
#define SYS_rseq 334
#endif
#define RSEQ_SIG 0x53053053

static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static int ready, go;
static __thread char dummy_area[32] __attribute__((aligned(32)));

static void fail(const char *msg) {
  printf("FAIL: %s\n", msg);
  ffs_assert(0, msg);
  fflush(stdout);
  abort();
}

static void check_rseq(const char *who) {
  errno = 0;
  long r = syscall(SYS_rseq, dummy_area, 32, 0, RSEQ_SIG);
  if (r == 0) {
    printf("%s: rseq registration succeeded\n", who);
    fail("thread lost rseq registration");
  }
  if (errno == EBUSY || errno == EINVAL || errno == EPERM || errno == ENOSYS)
    return;
  printf("%s: rseq returned %ld errno=%d\n", who, r, errno);
  fail("unexpected rseq result");
}

static void *worker(void *arg) {
  (void)arg;
  pthread_mutex_lock(&mu);
  ready = 1;
  pthread_cond_broadcast(&cv);
  while (!go) pthread_cond_wait(&cv, &mu); /* blocked here at the checkpoint */
  pthread_mutex_unlock(&mu);
  check_rseq("worker");
  return NULL;
}

int main(void) {
  pthread_t t;
  pthread_create(&t, NULL, worker, NULL);
  pthread_mutex_lock(&mu);
  while (!ready) pthread_cond_wait(&cv, &mu);
  pthread_mutex_unlock(&mu);

  ffs_checkpoint("rseq");

  check_rseq("main");
  pthread_mutex_lock(&mu);
  go = 1;
  pthread_cond_broadcast(&cv);
  pthread_mutex_unlock(&mu);
  pthread_join(t, NULL);
  printf("OK\n");
  return 0;
}
