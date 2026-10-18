/* readdir in a hot loop over a small directory: the kernel's filldir stops
 * after one entry when a signal is pending, so a preemption signal that
 * lands during getdents64 shortens its result.  A replay has to make the
 * same signal pending at the same syscall. */
#include <dirent.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>
#include "ffs.h"

int main(int argc, char **argv) {
  long rounds = argc > 1 ? atol(argv[1]) : 20000;
  char dir[] = "/tmp/ffs-dents-XXXXXX";
  if (!mkdtemp(dir)) return 1;
  for (int i = 0; i < 6; i++) {
    char p[64];
    snprintf(p, sizeof p, "%s/f%d", dir, i);
    close(open(p, O_CREAT | O_WRONLY, 0644));
  }
  long entries = 0, sum = 0;
  for (long r = 0; r < rounds; r++) {
    DIR *d = opendir(dir);
    struct dirent *e;
    while ((e = readdir(d))) {
      entries++;
      for (const char *c = e->d_name; *c; c++) sum += *c;
    }
    closedir(d);
  }
  for (int i = 0; i < 6; i++) {
    char p[64];
    snprintf(p, sizeof p, "%s/f%d", dir, i);
    unlink(p);
  }
  rmdir(dir);
  printf("%ld %ld\n", entries, sum);
  return 0;
}
