/* Long deterministic startup followed by a checkpoint and a short racy
 * region.  Demonstrates that exploration starts after the expensive phase. */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include "ffs.h"

#define barrier() __asm__ __volatile__("" ::: "memory")

static int counter;
static volatile unsigned long sink;

static void *worker(void *arg) {
  (void)arg;
  for (int i = 0; i < 100; i++) {
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
  /* Heavy but deterministic init: ~50M iterations of simple arithmetic. */
  unsigned long acc = 0;
  for (unsigned long i = 0; i < 50000000UL; i++) {
    acc = acc * 6364136223846793005UL + i;
    sink = acc;
  }
  printf("init done (%lu)\n", (unsigned long)(sink & 0xff));

  ffs_checkpoint("after-init");
  pthread_t t[2];
  ffs_begin();
  for (int i = 0; i < 2; i++) pthread_create(&t[i], NULL, worker, NULL);
  for (int i = 0; i < 2; i++) pthread_join(t[i], NULL);
  ffs_end();
  if (counter != 200) {
    printf("lost update: counter=%d\n", counter);
    ffs_assert(0, "lost update");
    fflush(stdout);
    abort();
  }
  printf("OK\n");
  return 0;
}
