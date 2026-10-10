/* fork + exec of /bin/true; the parent waits and reports the status.
 * Tests fork/exec handling in the tracer. */
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <unistd.h>
#include "ffs.h"

int main(void) {
  pid_t pid = fork();
  if (pid < 0) {
    perror("fork");
    return 1;
  }
  if (pid == 0) {
    execl("/bin/true", "true", (char *)NULL);
    execlp("true", "true", (char *)NULL);   /* fallback: no /bin/true on NixOS */
    _exit(127);
  }
  int status = 0;
  if (waitpid(pid, &status, 0) != pid) {
    perror("waitpid");
    return 1;
  }
  int code = WIFEXITED(status) ? WEXITSTATUS(status) : -1;
  printf("child status=%d\n", code);
  return 0;
}
