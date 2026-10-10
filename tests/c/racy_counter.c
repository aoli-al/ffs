/* Two threads increment a shared non-atomic counter without locking.
 * Natively this usually "works" because each thread finishes before the other
 * gets scheduled; a preemption between load and store loses an update. */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include "ffs.h"

#define barrier() __asm__ __volatile__("" ::: "memory")

static int counter;

static void *worker(void *arg) {
  (void)arg;
  for (int i = 0; i < 1000; i++) {
    int tmp = counter;
    barrier();
    tmp++;
    barrier();
    counter = tmp;
    barrier();
  }
  return NULL;
}

int main(void) {
  pthread_t t[2];
  ffs_begin();
  for (int i = 0; i < 2; i++) pthread_create(&t[i], NULL, worker, NULL);
  for (int i = 0; i < 2; i++) pthread_join(t[i], NULL);
  ffs_end();
  if (counter != 2000) {
    printf("lost update: counter=%d\n", counter);
    ffs_assert(0, "lost update");
    fflush(stdout);
    abort();
  }
  printf("OK\n");
  return 0;
}
