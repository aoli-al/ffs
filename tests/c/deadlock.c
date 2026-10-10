/* Classic ABBA lock-order deadlock.  Natively thread 1 usually completes
 * before thread 2 starts, so the program prints OK.  A native watchdog
 * (only armed when not under ffs) prevents an indefinite hang. */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include "ffs.h"

static pthread_mutex_t a = PTHREAD_MUTEX_INITIALIZER;
static pthread_mutex_t b = PTHREAD_MUTEX_INITIALIZER;
static int shared;

static void *t1(void *arg) {
  (void)arg;
  pthread_mutex_lock(&a);
  ffs_yield();
  pthread_mutex_lock(&b);
  shared++;
  pthread_mutex_unlock(&b);
  pthread_mutex_unlock(&a);
  return NULL;
}

static void *t2(void *arg) {
  (void)arg;
  pthread_mutex_lock(&b);
  ffs_yield();
  pthread_mutex_lock(&a);
  shared++;
  pthread_mutex_unlock(&a);
  pthread_mutex_unlock(&b);
  return NULL;
}

int main(void) {
  pthread_t x, y;
  if (!ffs_is_active()) alarm(10);   /* native-only watchdog: SIGALRM kills us */
  ffs_begin();
  pthread_create(&x, NULL, t1, NULL);
  pthread_create(&y, NULL, t2, NULL);
  pthread_join(x, NULL);
  pthread_join(y, NULL);
  ffs_end();
  if (shared != 2) {
    ffs_assert(0, "shared != 2");
    fflush(stdout);
    abort();
  }
  printf("OK\n");
  return 0;
}
