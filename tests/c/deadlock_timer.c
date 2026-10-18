/* ABBA deadlock in the presence of a periodic timer thread (like a JVM's
 * watcher threads): the process is never "all blocked without a deadline",
 * so the deadlock has to be inferred from the lack of progress.  With
 * "sleep" as argument the main thread sleeps 90 s before joining, which
 * must not be mistaken for a deadlock while the timer thread ticks. */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include "ffs.h"

static pthread_mutex_t a = PTHREAD_MUTEX_INITIALIZER;
static pthread_mutex_t b = PTHREAD_MUTEX_INITIALIZER;
static volatile int ticks_seen;
static volatile int done;

static void *timer(void *arg) {
  (void)arg;
  struct timespec ts = {0, 50 * 1000 * 1000}; /* 50 ms, like HotSpot's WatcherThread */
  while (!done) {
    nanosleep(&ts, NULL);
    ticks_seen++;
  }
  return NULL;
}

static void *t1(void *arg) {
  (void)arg;
  pthread_mutex_lock(&a);
  ffs_yield();
  pthread_mutex_lock(&b);
  pthread_mutex_unlock(&b);
  pthread_mutex_unlock(&a);
  return NULL;
}

static void *t2(void *arg) {
  (void)arg;
  pthread_mutex_lock(&b);
  ffs_yield();
  pthread_mutex_lock(&a);
  pthread_mutex_unlock(&a);
  pthread_mutex_unlock(&b);
  return NULL;
}

int main(int argc, char **argv) {
  pthread_t x, y, w;
  int sleepy = argc > 1 && strcmp(argv[1], "sleep") == 0;
  if (!ffs_is_active()) alarm(10);
  pthread_create(&w, NULL, timer, NULL);
  ffs_begin();
  if (sleepy) {
    /* A long one-shot sleep while only the timer thread runs. */
    struct timespec ts = {90, 0};
    nanosleep(&ts, NULL);
    puts("woke up");
    done = 1;
    pthread_join(w, NULL);
    ffs_end();
    puts("OK");
    return 0;
  }
  pthread_create(&x, NULL, t1, NULL);
  pthread_create(&y, NULL, t2, NULL);
  pthread_join(x, NULL);
  pthread_join(y, NULL);
  done = 1;
  pthread_join(w, NULL);
  ffs_end();
  puts("OK");
  return 0;
}
