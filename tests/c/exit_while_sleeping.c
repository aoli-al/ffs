// The main thread exits (exit_group) while another thread sleeps for a
// very long time: the run ends with the exit status, at the exit's
// virtual time, not after a clock jump to the sleeper's deadline (which
// would exceed --max-vtime and be reported as a hang), and the tracer
// must not touch the killed sleeper's registers.
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <time.h>
#include <unistd.h>

static void *sleeper(void *arg) {
    (void)arg;
    struct timespec ts = {10000, 0};
    nanosleep(&ts, NULL);
    return NULL;
}

int main(void) {
    pthread_t t;
    pthread_create(&t, NULL, sleeper, NULL);
    struct timespec ts = {0, 1000 * 1000};
    nanosleep(&ts, NULL);
    printf("ok\n");
    exit(0);
}
