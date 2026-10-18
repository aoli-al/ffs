/* Fresh pages touched inside a branchy loop, several threads: a probe for
 * whether the retired-conditional-branch counter is exactly reproducible
 * across page faults and the interrupts they bring. */
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include "ffs.h"

static long pages = 50000;

static void *worker(void *arg) {
  long id = (long)arg;
  size_t len = (size_t)pages * 4096;
  char *m = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (m == MAP_FAILED) return NULL;
  long sum = 0;
  for (long p = 0; p < pages; p++) {
    char *q = m + p * 4096;
    for (int i = 0; i < 64; i++) {
      q[i * 61 % 4096] = (char)(i + id);
      if (q[i] & 1) sum++;
    }
  }
  munmap(m, len);
  return (void *)sum;
}

int main(int argc, char **argv) {
  if (argc > 1) pages = atol(argv[1]);
  pthread_t t[4];
  for (long i = 0; i < 4; i++) pthread_create(&t[i], NULL, worker, (void *)i);
  long total = 0;
  for (int i = 0; i < 4; i++) { void *r; pthread_join(t[i], &r); total += (long)r; }
  printf("%ld\n", total);
  return 0;
}
