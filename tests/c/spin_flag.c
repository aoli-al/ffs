/* Thread A busy-waits (bounded) on a flag that thread B sets.  Under a
 * scheduler that never preempts A, the bound is reached and we report it.
 * Tests preemption of a busy loop by the tick counter. */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include "ffs.h"

#define MAX_SPINS 100000000UL

static int flag;

static void *spinner(void *arg) {
  (void)arg;
  unsigned long spins = 0;
  while (!__atomic_load_n(&flag, __ATOMIC_ACQUIRE)) {
    if (++spins >= MAX_SPINS) {
      printf("spin bound reached without seeing flag\n");
      ffs_assert(0, "spinner starved: flag never observed");
      fflush(stdout);
      abort();
    }
  }
  return NULL;
}

static void *setter(void *arg) {
  (void)arg;
  __atomic_store_n(&flag, 1, __ATOMIC_RELEASE);
  return NULL;
}

int main(void) {
  pthread_t a, b;
  ffs_begin();
  pthread_create(&a, NULL, spinner, NULL);
  pthread_create(&b, NULL, setter, NULL);
  pthread_join(a, NULL);
  pthread_join(b, NULL);
  ffs_end();
  printf("OK\n");
  return 0;
}
