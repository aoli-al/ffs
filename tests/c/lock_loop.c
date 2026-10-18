/* Lock-prefixed instructions in a hot loop, several threads: a probe for
 * whether the retired-conditional-branch counter is exactly reproducible
 * around them (AMD Zen's SpecLockMap optimization is known to break this
 * for rr, which requires it disabled via MSR). */
#include <pthread.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include "ffs.h"

static atomic_long counter;
static long iters = 2000000;

static void *worker(void *arg) {
  long id = (long)arg;
  long local = 0;
  for (long i = 0; i < iters; i++) {
    long old = atomic_load(&counter);
    while (!atomic_compare_exchange_weak(&counter, &old, old + 1)) local++;
    if ((i & 0xff) == id) atomic_fetch_add(&counter, 0);
  }
  return (void *)local;
}

int main(int argc, char **argv) {
  if (argc > 1) iters = atol(argv[1]);
  pthread_t t[4];
  for (long i = 0; i < 4; i++) pthread_create(&t[i], NULL, worker, (void *)i);
  for (int i = 0; i < 4; i++) pthread_join(t[i], NULL);
  printf("%ld\n", (long)atomic_load(&counter));
  return 0;
}
