/* A periodic timerfd read through epoll, as Netty's native transport and
 * event loops use them: under ffs the expirations happen on the virtual
 * clock (instantly in real time) and the count and timings are exact. */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/epoll.h>
#include <sys/timerfd.h>
#include <time.h>
#include <unistd.h>
#include "ffs.h"

static long now_ms(void) {
  struct timespec ts;
  clock_gettime(CLOCK_MONOTONIC, &ts);
  return ts.tv_sec * 1000 + ts.tv_nsec / 1000000;
}

int main(void) {
  int tfd = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK | TFD_CLOEXEC);
  if (tfd < 0) { perror("timerfd_create"); return 1; }
  struct itimerspec its = {{0, 20 * 1000000}, {0, 20 * 1000000}};
  if (timerfd_settime(tfd, 0, &its, NULL) < 0) { perror("settime"); return 1; }
  int ep = epoll_create1(0);
  struct epoll_event ev = {EPOLLIN, {.fd = tfd}};
  epoll_ctl(ep, EPOLL_CTL_ADD, tfd, &ev);
  long start = now_ms();
  uint64_t total = 0;
  int wakeups = 0;
  while (total < 5) {
    struct epoll_event out;
    int n = epoll_wait(ep, &out, 1, 5000);
    if (n != 1) { printf("epoll_wait returned %d\n", n); return 1; }
    uint64_t count;
    if (read(tfd, &count, 8) != 8) { perror("read"); return 1; }
    total += count;
    wakeups++;
  }
  long elapsed = now_ms() - start;
  struct itimerspec cur;
  timerfd_gettime(tfd, &cur);
  if (cur.it_interval.tv_nsec != 20 * 1000000) { printf("gettime interval %ld\n", cur.it_interval.tv_nsec); return 1; }
  its.it_value.tv_nsec = 0;
  timerfd_settime(tfd, 0, &its, NULL);  /* disarm */
  close(tfd);
  close(ep);
  if (elapsed < 100 || elapsed > 200) { printf("elapsed %ld ms for 5 x 20 ms\n", elapsed); return 1; }
  printf("OK %d wakeups %lu expirations %ld ms\n", wakeups, (unsigned long)total, elapsed);
  return 0;
}
