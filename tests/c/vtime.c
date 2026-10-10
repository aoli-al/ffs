/* Everything time-related must be deterministic and must not wait for real
 * time under ffs: sleeps, an interval timer, a POSIX timer, a timed condvar
 * wait, poll with a timeout, alarm().  The output must be identical across
 * runs, and the whole program (which sleeps for ~3 virtual seconds) must
 * finish almost instantly. */
#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>
#include "ffs.h"

static volatile int alarms, usr1s, usr1_tid_ok;
static pthread_mutex_t mu = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;

static long ms_since(const struct timespec *a) {
  struct timespec b;
  clock_gettime(CLOCK_MONOTONIC, &b);
  return (b.tv_sec - a->tv_sec) * 1000 + (b.tv_nsec - a->tv_nsec) / 1000000;
}

static void on_alarm(int sig) { (void)sig; alarms++; }
static void on_usr1(int sig, siginfo_t *si, void *ctx) {
  (void)sig; (void)ctx;
  usr1s++;
  usr1_tid_ok = si->si_code == SI_TIMER && si->si_value.sival_int == 42;
}

static void *sleeper(void *arg) {
  (void)arg;
  struct timespec t0;
  clock_gettime(CLOCK_MONOTONIC, &t0);
  usleep(200000);
  printf("sleeper: usleep(200ms) took %ld ms\n", ms_since(&t0));
  return NULL;
}

int main(void) {
  struct timespec t0;
  clock_gettime(CLOCK_MONOTONIC, &t0);

  /* sleeps */
  struct timespec req = {0, 50000000}, rem = {0, 0};
  nanosleep(&req, &rem);
  printf("nanosleep(50ms) -> %ld ms\n", ms_since(&t0));
  sleep(1);
  printf("sleep(1) -> %ld ms\n", ms_since(&t0));

  /* interval timer: 1ms period for a loop of 20M iterations */
  struct sigaction sa;
  memset(&sa, 0, sizeof sa);
  sa.sa_handler = on_alarm;
  sa.sa_flags = SA_RESTART;
  sigaction(SIGALRM, &sa, NULL);
  struct itimerval it = {{0, 1000}, {0, 1000}};
  setitimer(ITIMER_REAL, &it, NULL);
  volatile unsigned long acc = 1;
  for (unsigned long i = 0; i < 20000000UL; i++) acc = acc * 6364136223846793005UL + 1;
  memset(&it, 0, sizeof it);
  setitimer(ITIMER_REAL, &it, NULL);
  printf("alarms=%d\n", alarms);

  /* POSIX one-shot timer with a value */
  memset(&sa, 0, sizeof sa);
  sa.sa_sigaction = on_usr1;
  sa.sa_flags = SA_SIGINFO;
  sigaction(SIGUSR1, &sa, NULL);
  timer_t tm;
  struct sigevent sev;
  memset(&sev, 0, sizeof sev);
  sev.sigev_notify = SIGEV_SIGNAL;
  sev.sigev_signo = SIGUSR1;
  sev.sigev_value.sival_int = 42;
  timer_create(CLOCK_MONOTONIC, &sev, &tm);
  struct itimerspec its = {{0, 0}, {0, 2000000}};
  timer_settime(tm, 0, &its, NULL);
  usleep(10000);
  printf("usr1s=%d siginfo_ok=%d\n", usr1s, usr1_tid_ok);

  /* timed condvar wait that must time out, and poll with a timeout */
  struct timespec dl;
  clock_gettime(CLOCK_REALTIME, &dl);
  dl.tv_sec += 1;
  pthread_mutex_lock(&mu);
  int r = pthread_cond_timedwait(&cv, &mu, &dl);
  pthread_mutex_unlock(&mu);
  printf("cond_timedwait(1s) -> %s after %ld ms\n", r == ETIMEDOUT ? "ETIMEDOUT" : "?", ms_since(&t0));
  r = poll(NULL, 0, 300);
  printf("poll(300ms) -> %d after %ld ms\n", r, ms_since(&t0));

  /* a sleeping thread and alarm() */
  pthread_t th;
  pthread_create(&th, NULL, sleeper, NULL);
  pthread_join(th, NULL);
  unsigned left = alarm(5);
  printf("alarm(5) previous=%u, remaining=%u\n", left, alarm(0));
  printf("total virtual time: %ld ms\n", ms_since(&t0));
  return 0;
}
