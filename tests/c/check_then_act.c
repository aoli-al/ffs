/* TOCTOU on a shared pointer.  The "user" thread checks p under the lock,
 * drops the lock, and later uses p again.  The "freer" thread clears and
 * frees p in between.  Natively the user thread almost always finishes
 * before the freer thread even starts. */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include "ffs.h"

static int *p;
static pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER;

static void *user(void *arg) {
  (void)arg;
  pthread_mutex_lock(&m);
  int ok = (p != NULL);              /* check */
  pthread_mutex_unlock(&m);
  if (ok) {
    ffs_yield();                     /* window: freer may run here */
    pthread_mutex_lock(&m);
    int *q = p;                      /* act */
    if (q == NULL) {
      printf("pointer freed between check and use\n");
      ffs_assert(0, "check-then-act: p became NULL");
      fflush(stdout);
      abort();
    }
    *q = 1;
    pthread_mutex_unlock(&m);
  }
  return NULL;
}

static void *freer(void *arg) {
  (void)arg;
  ffs_yield();
  pthread_mutex_lock(&m);
  int *q = p;
  p = NULL;
  pthread_mutex_unlock(&m);
  free(q);
  return NULL;
}

int main(void) {
  pthread_t a, b;
  p = malloc(sizeof *p);
  *p = 0;
  ffs_begin();
  pthread_create(&a, NULL, user, NULL);
  pthread_create(&b, NULL, freer, NULL);
  pthread_join(a, NULL);
  pthread_join(b, NULL);
  ffs_end();
  printf("OK\n");
  return 0;
}
