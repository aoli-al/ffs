/* Process/thread identity must survive a checkpoint: a faithful copy keeps
 * the same pid/tids, still owns a recursive mutex locked before the
 * checkpoint, and still has the worker thread blocked on its condvar. */
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include "ffs.h"

static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER; /* ready/go handshake */
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static pthread_mutex_t rec; /* recursive, held by main across the checkpoint */
static int ready, go;
static pid_t worker_tid;
static pthread_t worker_self;

static void fail(const char *msg) {
  printf("FAIL: %s\n", msg);
  ffs_assert(0, msg);
  fflush(stdout);
  abort();
}

static void *worker(void *arg) {
  (void)arg;
  pthread_mutex_lock(&mu);
  worker_tid = gettid();
  worker_self = pthread_self();
  ready = 1;
  pthread_cond_broadcast(&cv);
  while (!go) pthread_cond_wait(&cv, &mu); /* blocked here at the checkpoint */
  pthread_mutex_unlock(&mu);
  if (gettid() != worker_tid) fail("worker tid changed after checkpoint");
  if (!pthread_equal(pthread_self(), worker_self))
    fail("worker pthread_self changed after checkpoint");
  return NULL;
}

int main(void) {
  pid_t pid = getpid(), tid = gettid();
  pthread_mutexattr_t attr;
  pthread_mutexattr_init(&attr);
  pthread_mutexattr_settype(&attr, PTHREAD_MUTEX_RECURSIVE);
  pthread_mutex_init(&rec, &attr);
  pthread_mutex_lock(&rec);

  pthread_t t;
  pthread_create(&t, NULL, worker, NULL);
  pthread_mutex_lock(&mu);
  while (!ready) pthread_cond_wait(&cv, &mu);
  pthread_mutex_unlock(&mu);

  ffs_checkpoint("tids");

  if (getpid() != pid) fail("getpid changed after checkpoint");
  if (gettid() != tid) fail("gettid changed after checkpoint");
  if (pthread_mutex_trylock(&rec) != 0) fail("recursive mutex ownership lost");
  pthread_mutex_unlock(&rec);
  pthread_mutex_unlock(&rec);
  if (pthread_kill(t, 0) != 0) fail("worker thread vanished after checkpoint");

  pthread_mutex_lock(&mu);
  go = 1;
  pthread_cond_broadcast(&cv);
  pthread_mutex_unlock(&mu);
  pthread_join(t, NULL);
  printf("OK\n");
  return 0;
}
