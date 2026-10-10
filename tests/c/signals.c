/* Single-threaded: a 1ms SIGALRM interval timer ticks while a 20M-iteration
 * arithmetic loop runs.  Prints the number of alarms observed. */
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/time.h>
#include "ffs.h"

static volatile int alarms;
static volatile unsigned long sink;

static void on_alarm(int sig) {
  (void)sig;
  alarms++;
}

int main(void) {
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_handler = on_alarm;
  sa.sa_flags = SA_RESTART;
  sigaction(SIGALRM, &sa, NULL);

  struct itimerval it;
  it.it_interval.tv_sec = 0;
  it.it_interval.tv_usec = 1000;
  it.it_value = it.it_interval;
  setitimer(ITIMER_REAL, &it, NULL);

  unsigned long acc = 1;
  for (unsigned long i = 0; i < 20000000UL; i++) {
    acc = acc * 2862933555777941757UL + 3037000493UL;
    sink = acc;
  }

  memset(&it, 0, sizeof it);
  setitimer(ITIMER_REAL, &it, NULL);  /* disable */
  printf("alarms=%d\n", alarms);
  return 0;
}
