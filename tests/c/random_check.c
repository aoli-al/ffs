/* Prints 16 bytes from getrandom() and 16 bytes from /dev/urandom as hex.
 * Used to check that ffs virtualizes randomness: two runs under ffs must
 * print identical lines.  Natively the lines differ from run to run. */
#include <stdio.h>
#include <stdlib.h>
#include <sys/random.h>
#include <unistd.h>
#include <fcntl.h>

static void print_hex(const char *label, const unsigned char *b, size_t n) {
  printf("%s=", label);
  for (size_t i = 0; i < n; i++) printf("%02x", b[i]);
  printf("\n");
}

int main(void) {
  unsigned char buf[16];
  if (getrandom(buf, sizeof buf, 0) != (ssize_t)sizeof buf) {
    perror("getrandom");
    return 1;
  }
  print_hex("getrandom", buf, sizeof buf);

  int fd = open("/dev/urandom", O_RDONLY);
  if (fd < 0 || read(fd, buf, sizeof buf) != (ssize_t)sizeof buf) {
    perror("/dev/urandom");
    return 1;
  }
  close(fd);
  print_hex("urandom", buf, sizeof buf);
  return 0;
}
