/* A race whose window has no conditional branch, syscall or call: the
 * setter stores a then b back to back, and the checker must observe the
 * state in between (a == 1 && b == 0).  Tick-counted preemption can never
 * stop between the two stores; a watchpoint on `a` can. */
#include <pthread.h>
#include <stdio.h>
#include "ffs.h"

volatile int a, b;

static void *setter(void *arg) {
  (void)arg;
  a = 1;
  b = -1;
  return NULL;
}

static void *checker(void *arg) {
  (void)arg;
  int x = a;
  int y = b;
  ffs_assert(!(x == 1 && y == 0), "checker saw a == 1 && b == 0");
  return NULL;
}

int main(void) {
  pthread_t s, c;
  pthread_create(&c, NULL, checker, NULL);
  pthread_create(&s, NULL, setter, NULL);
  pthread_join(s, NULL);
  pthread_join(c, NULL);
  printf("done\n");
  return 0;
}
