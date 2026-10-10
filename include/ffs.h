/* ffs.h - optional tracee-side API for programs tested under ffs.
 *
 * Every entry point is a raw syscall with a number the kernel does not know
 * (it returns -ENOSYS when the program runs natively).  When the program runs
 * under ffs, the tracer intercepts the syscall-entry stop, performs the action
 * and rewrites the return value.  So all calls are harmless no-ops outside
 * ffs, and the header has no link-time dependency.
 */
#ifndef FFS_H
#define FFS_H

#include <stddef.h>
#include <unistd.h>
#include <sys/syscall.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Syscall numbers reserved by ffs.  They are far above anything Linux uses. */
#define FFS_SYS_BASE        0x0ff5000L
#define FFS_SYS_CHECKPOINT  (FFS_SYS_BASE + 1)  /* (const char* name) -> 0 */
#define FFS_SYS_ASSERT      (FFS_SYS_BASE + 2)  /* (long cond, const char* msg) -> 0 */
#define FFS_SYS_YIELD       (FFS_SYS_BASE + 3)  /* () -> 0 ; scheduling point */
#define FFS_SYS_LOG         (FFS_SYS_BASE + 4)  /* (const char* msg) -> 0 */
#define FFS_SYS_ACTIVE      (FFS_SYS_BASE + 5)  /* () -> 1 under ffs, -ENOSYS otherwise */
#define FFS_SYS_BEGIN       (FFS_SYS_BASE + 6)  /* () -> 0 ; start of the region to explore */
#define FFS_SYS_END         (FFS_SYS_BASE + 7)  /* () -> 0 ; end of the region to explore */

/* Create a checkpoint of the whole process here.  Under `ffs explore` the
 * exploration tree is rooted at the most recent checkpoint; under `ffs record`
 * it is a no-op apart from being noted in the trace. */
static inline long ffs_checkpoint(const char* name) {
  return syscall(FFS_SYS_CHECKPOINT, name);
}

/* Report a property violation to ffs.  If `cond` is zero the current branch is
 * reported as failing (the schedule is saved) and the program is killed. */
static inline long ffs_assert(long cond, const char* msg) {
  return syscall(FFS_SYS_ASSERT, cond, msg);
}

/* Hint to the scheduler that this is a good place to switch threads. */
static inline long ffs_yield(void) {
  return syscall(FFS_SYS_YIELD);
}

/* Print a message through the tracer (prefixed with the thread id). */
static inline long ffs_log(const char* msg) {
  return syscall(FFS_SYS_LOG, msg);
}

/* 1 when running under ffs, 0 otherwise. */
static inline int ffs_is_active(void) {
  return syscall(FFS_SYS_ACTIVE) == 1;
}

/* Mark the beginning / end of the interesting region.  `ffs explore` creates a
 * checkpoint at ffs_begin() if no explicit ffs_checkpoint() was requested, and
 * stops exploring a branch at ffs_end(). */
static inline long ffs_begin(void) { return syscall(FFS_SYS_BEGIN); }
static inline long ffs_end(void) { return syscall(FFS_SYS_END); }

#ifdef __cplusplus
}
#endif

#endif /* FFS_H */
