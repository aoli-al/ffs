/* Four threads each bump a racy counter and a correctly locked counter.
 * The locked counter is always right; the racy one may lose updates. */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include "ffs.h"

#define barrier() __asm__ __volatile__("" ::: "memory")
#define NTHREADS 4
#define ITERS 500

static int racy;
static int locked;
static pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER;

static void *worker(void *arg) {
  (void)arg;
  for (int i = 0; i < ITERS; i++) {
    int tmp = racy;
    barrier();
    tmp++;
    barrier();
    racy = tmp;
    barrier();

    pthread_mutex_lock(&m);
    locked++;
    pthread_mutex_unlock(&m);
  }
  return NULL;
}

int main(void) {
  pthread_t t[NTHREADS];
  ffs_begin();
  for (int i = 0; i < NTHREADS; i++) pthread_create(&t[i], NULL, worker, NULL);
  for (int i = 0; i < NTHREADS; i++) pthread_join(t[i], NULL);
  ffs_end();
  printf("racy=%d locked=%d\n", racy, locked);
  if (racy != NTHREADS * ITERS) {
    ffs_assert(0, "lost update on racy counter");
    fflush(stdout);
    abort();
  }
  printf("OK\n");
  return 0;
}
