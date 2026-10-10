/* MAP_SHARED pages (anonymous and memfd-backed) must be private to each
 * branch.  If the pages were still shared with the original process or with
 * other branches, a branch would observe more increments than its own. */
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <unistd.h>
#include "ffs.h"

static void fail(const char *msg) {
  printf("FAIL: %s\n", msg);
  ffs_assert(0, msg);
  fflush(stdout);
  abort();
}

int main(void) {
  long psz = sysconf(_SC_PAGESIZE);
  volatile int *anon = mmap(NULL, psz, PROT_READ | PROT_WRITE,
                            MAP_SHARED | MAP_ANONYMOUS, -1, 0);
  if (anon == MAP_FAILED) { perror("mmap anon"); return 1; }
  int fd = memfd_create("ffs-shared-map", 0);
  if (fd < 0 || ftruncate(fd, psz) != 0) { perror("memfd_create"); return 1; }
  volatile int *file = mmap(NULL, psz, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
  if (file == MAP_FAILED) { perror("mmap memfd"); return 1; }
  anon[0] = 0;
  file[0] = 0;

  ffs_checkpoint("shm");

  for (int i = 0; i < 1000; i++) {
    anon[0]++;
    file[0]++;
  }
  if (anon[0] != 1000) {
    printf("anon page counter=%d\n", anon[0]);
    fail("shared anonymous page leaked across branches");
  }
  if (file[0] != 1000) {
    printf("memfd page counter=%d\n", file[0]);
    fail("memfd-backed shared page leaked across branches");
  }
  printf("OK\n");
  return 0;
}
