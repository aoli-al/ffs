/* Producer/consumer where the consumer guards pthread_cond_wait with `if`
 * instead of `while`.  With two consumers woken by one broadcast, the second
 * one to reacquire the mutex proceeds although the queue is already empty. */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>
#include "ffs.h"

static pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static int count;

static void *producer(void *arg) {
  (void)arg;
  for (int i = 0; i < 2; i++) {
    ffs_yield();
    pthread_mutex_lock(&m);
    count++;
    pthread_cond_broadcast(&cv);
    pthread_mutex_unlock(&m);
  }
  return NULL;
}

static void *consumer(void *arg) {
  (void)arg;
  pthread_mutex_lock(&m);
  if (count == 0)                    /* BUG: should be `while` */
    pthread_cond_wait(&cv, &m);
  if (count <= 0) {
    printf("consumed from empty queue (count=%d)\n", count);
    ffs_assert(0, "consumed from empty queue");
    fflush(stdout);
    abort();
  }
  count--;
  pthread_mutex_unlock(&m);
  return NULL;
}

int main(void) {
  pthread_t c1, c2, p;
  if (!ffs_is_active()) alarm(10);   /* native-only watchdog */
  ffs_begin();
  pthread_create(&c1, NULL, consumer, NULL);
  pthread_create(&c2, NULL, consumer, NULL);
  pthread_create(&p, NULL, producer, NULL);
  pthread_join(c1, NULL);
  pthread_join(c2, NULL);
  pthread_join(p, NULL);
  ffs_end();
  if (count != 0) {
    ffs_assert(0, "queue not drained");
    fflush(stdout);
    abort();
  }
  printf("OK\n");
  return 0;
}
