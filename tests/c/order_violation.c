/* Order violation: thread B uses `cfg` assuming thread A has initialised it,
 * but nothing enforces that order.  Natively A (created first) almost always
 * wins the race. */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include "ffs.h"

struct cfg { int a; int b; };
static struct cfg *cfg;
static int initialized;

static void *init_thread(void *arg) {
  (void)arg;
  struct cfg *c = malloc(sizeof *c);
  c->a = 1;
  c->b = 2;
  cfg = c;
  __atomic_store_n(&initialized, 1, __ATOMIC_RELEASE);
  return NULL;
}

static void *use_thread(void *arg) {
  (void)arg;
  /* Unrelated bounded setup work; natively this is what lets A win the race. */
  volatile unsigned long busy = 0;
  for (unsigned long i = 0; i < 1000000UL; i++) busy += i;
  ffs_yield();                       /* scheduler may (or may not) run A here */
  if (!__atomic_load_n(&initialized, __ATOMIC_ACQUIRE)) {
    printf("cfg used before init\n");
    ffs_assert(0, "used before init");
    fflush(stdout);
    abort();
  }
  volatile int sum = cfg->a + cfg->b;
  (void)sum;
  return NULL;
}

int main(void) {
  pthread_t a, b;
  ffs_begin();
  pthread_create(&a, NULL, init_thread, NULL);
  pthread_create(&b, NULL, use_thread, NULL);
  pthread_join(a, NULL);
  pthread_join(b, NULL);
  ffs_end();
  free(cfg);
  printf("OK\n");
  return 0;
}
