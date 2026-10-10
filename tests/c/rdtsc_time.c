/* Single-threaded: measures rdtsc and CLOCK_MONOTONIC deltas across a small
 * loop.  Used to check that the tool virtualises time. */
#include <stdio.h>
#include <time.h>
#include <x86intrin.h>
#include "ffs.h"

static volatile unsigned long sink;

int main(void) {
  struct timespec t0, t1;
  unsigned long long c0 = __rdtsc();
  clock_gettime(CLOCK_MONOTONIC, &t0);
  for (unsigned long i = 0; i < 1000000UL; i++) sink += i;
  unsigned long long c1 = __rdtsc();
  clock_gettime(CLOCK_MONOTONIC, &t1);
  long ns = (t1.tv_sec - t0.tv_sec) * 1000000000L + (t1.tv_nsec - t0.tv_nsec);
  printf("rdtsc delta=%llu\n", c1 - c0);
  printf("clock delta=%ld ns\n", ns);
  ffs_log("rdtsc_time done");
  return 0;
}
