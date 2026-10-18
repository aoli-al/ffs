# Popular Rust concurrency crates under ffs

Can `ffs` test widely used concurrent software as it is?  This directory
runs the *unmodified* test suites of four of the most downloaded Rust
concurrency crates under `ffs explore`, and reproduces two reported
concurrency bugs of theirs with the code before and after the fix.

| crate (commit, Oct 2026) | what it is | tests | schedules each | result |
|---|---|---|---|---|
| crossbeam `099d046` (channel, deque, epoch, queue, skiplist, utils) | lock-free channels, work-stealing deques, epoch GC, skiplist | 658 | 20 | 655 pass; 3 stress tests time out (ffs throughput, see below) |
| tokio `cdd9dae` (`--features full`) | async runtime: work-stealing scheduler, sync primitives, I/O, process, signals, timers | 1575 | 20 | 1569 pass; 6 process tests failed for ffs reasons, fixed below (now 1575/1575) |
| parking_lot `46b4f58` (+ lock_api, parking_lot_core) | mutex/rwlock/condvar/once built on a parking lot | 127 | 20 | 127 pass |
| dashmap `366ce7e` (v7.0.0-rc2) | sharded concurrent hash map | 33 | 20 | 33 pass |
| parking_lot `093e28e` (before PR #533's synchronization fixes) | the suite that shipped before `WordLock`, rwlock-upgrade and fair-unlock fixes | 105 | 20 | 105 pass (the fixed bugs had no test) |

No new library bug was found; the schedules ffs explored did not shake a
failure out of code that is already fuzzed by loom/shuttle/Miri/TSan
upstream.  What the sweep did do is exercise ffs on ~2,400 real
multithreaded programs (tokio's multi-thread runtime with epoll, eventfd,
timers, pipes, TCP/UDP/Unix sockets, child processes and signals;
crossbeam's spinning lock-free structures with up to 4+ worker threads)
and find two gaps in ffs's process-tree emulation, fixed in this change.

## Running it

```
./run.py build crossbeam tokio parking_lot dashmap     # cargo test --no-run, release + debug assertions
./run.py list tokio --bin sync_notify -v               # binaries and tests
./run.py run crossbeam --iterations 20 -j 8            # every test, 20 schedules each
./run.py run tokio --bin rt_threaded --test spawn_blocking --iterations 200
./run.py summary results/explore/crossbeam             # non-PASS rows + counts
```

`build` expects checkouts under `~/repos/crates-src/<crate>` (plain `git
clone`s; nothing is patched) and records the test binaries cargo built
in `build/<crate>.json`.  `run` executes each test as its own ffs session,
`<binary> --exact <test> --test-threads=1 --nocapture`, explored from the
start (these tests are milliseconds long, so no checkpoint is needed),
with a per-test seed, a private working directory and `TMPDIR` (temp-file
names are deterministic under ffs's virtual randomness, so parallel
sessions sharing `/tmp` would collide), and a copy of the package's
`Cargo.toml` (tokio's `basic_fs` reads it).  A test fails when some
schedule panics (exit 101), crashes, deadlocks, hangs or exceeds the step
bound; the log and the failing schedules stay in
`results/<mode>/<crate>/<binary>/<test>/`.  Speed: 0.3–3 s per test for 20
schedules; the whole tokio suite takes about 40 minutes at `-j 8`.

Three crossbeam stress tests (`array_queue::{mpmc,spsc}`,
`select::forget_select_bounded`, 25k–100k operations on spinning
lock-free queues from 4 threads) time out: one schedule takes 80–400 s
and hits the 100k-step depth bound, so they are a throughput limit of ffs
(every step is a ptrace stop), not a finding.  They pass with
`--iterations 2 --timeout 3600`.

## Known bugs reproduced (`known.py`)

```
./known.py setup            # git worktrees at fix^ and fix, harness builds
./known.py native --runs 50 # the plain program, for comparison
./known.py run --iterations 1000
```

`known/<name>/src/main.rs` performs the scenario of a bug report once;
`known.py` builds it against the commit before the fix and the fix
itself (worktrees under `~/repos/crates-src/<name>-{pre,post}`) and
explores both from the start.

### tokio #7922: blocking pool shutdown race (fixed Feb 2026)

`spawn_blocking` on a pool with one idle thread decrements
`num_idle_threads` and notifies the thread; if the runtime is dropped
before the thread wakes, the thread sees `shutdown`, exits, and decrements
the counter again: `assertion failed: num_idle_threads underflowed on
thread exit` in the blocking thread (tokio's own loom test
`spawn_blocking_then_shutdown`, here on real threads).

| | native (50 runs) | ffs explore (1000 schedules) |
|---|---|---|
| before the fix (`f21f2693^`) | 50/50 fail | 143/1000 fail (14%) |
| fixed (`f21f2693`) | 0/50 | 0 |

The native rate is *higher* than ffs's: the OS schedule nearly always
lets the main thread reach `drop(rt)` while the woken blocking thread is
still coming out of its futex wait (tens of microseconds), whereas ffs's
random policy runs the woken thread first about half the time.  A
preemption-heavy policy is not automatically the most adversarial one for
a wakeup-latency race.

### parking_lot #212: recursive `read()` against a waiting `write()`

A thread holding a read lock takes a second `read()` while another thread
waits in `write()`: parking_lot's readers wait behind a waiting writer,
so the second read blocks on the writer, which blocks on the first read.
Reported in 2019 with a 1-second spin loop; closed by PR #533 (Oct 2026),
which documents it and adds `RecursiveRwLock` rather than changing
`read()`.

| | native (20 rounds, 20 runs) | native (200k rounds) | ffs explore (1000 schedules, 20 rounds) |
|---|---|---|---|
| before PR #533 (`6be7fac^`) | 0 hang | deadlocks within 5 s (5/5) | 5/1000 deadlocks |
| PR #533 (`6be7fac`) | 0 hang | deadlocks within 5 s (5/5) | 7/1000 deadlocks |

ffs reports the deadlock (`FAILURE deadlock (blocked threads [1, 2, 3])`)
with a reproducible schedule after 20 rounds, where the native program
needs hundreds of thousands.  Both sides deadlock, as the maintainers'
resolution implies.

## Going deeper (second pass)

The 20-schedule sweep is a smoke test, so the second pass spent its time
where races live:

* **300 schedules per test** on the concurrency-heavy subset of the
  suites (crossbeam's channel/deque/queue/skiplist/epoch/utils tests,
  tokio's sync/runtime/task/time tests and its lib tests, all of
  parking_lot and dashmap), in `results/deep/`.  No failure other than
  throughput timeouts (100-thread parking_lot tests, crossbeam stress
  tests, flume's `send_bounded`, tokio's large-buffer fs tests: 300
  schedules of those exceed 30 minutes).
* **Three less-tested crates** at 100 schedules per test: papaya (lock-free
  hash map, 100 tests), flume (channels, 238 tests), concurrent-queue (31
  tests).  All pass.
* **Stress harnesses with invariants** that the unit tests do not check
  together (`known/*-stress`, `known/tokio-notify-watch`): dashmap 7 rc2
  under insert/entry/remove/retain/iter/shrink from 3–4 threads
  (increments accounted for, `len()` vs iteration); crossbeam-channel
  `select!` over zero/bounded/unbounded channels with timeouts and early
  disconnection (every message exactly once); tokio Notify, Semaphore,
  broadcast, bounded mpsc and watch on a 2-worker runtime (permits neither
  lost nor duplicated, ordering per sender, lag accounting); tokio Notify
  with a hardware watchpoint on its state word (`ffs_watch` of a
  `#[no_mangle]` static) and `--watch-hold`.  300–500 schedules each,
  random and PCT policies: no failure.
* **Two more fixed bugs reproduced** from parking_lot's PR #533, both of
  which had no test and no minimal reproducer:

  | | native | ffs explore, before fix | ffs explore, after fix |
  |---|---|---|---|
  | #505: `read_recursive()` parked after a parked writer is never woken (4 threads: a writer holding, a reader that will wait on a condvar while holding its read lock, a second writer, the recursive reader) | 7/20 runs hang | 275/500 deadlock | 0/500 (RecursiveRwLock) |
  | `unlock_upgradable_fair` not handing off to the parked upgradable reader (`unlock_upgradable_slow(false)`) | 1/50 fail | 182/300 and 279/500 fail | 0/300, 0/500 |

  The #505 harness is the first minimal reproducer of that issue (the
  report had a game-engine workload that deadlocked after minutes with
  16 threads).  The fair-unlock one needs a warm-up round: a bucket's
  eventual-fairness timer starts expired, so the first fair unlock of a
  process always hands off even without the fix, and a `sleep` while the
  other thread parks would expire it again.

Mistakes worth recording, because each looked like a bug for a moment:
`notify_one` coalesces permits when nobody waits, so a harness that
counts concurrent `notify_one` calls hangs natively (one notification in
flight at a time is the sound version); a consumer polling a disconnected
channel without rotating livelocks; and parking_lot spins with yields
about ten times before parking, so a thread "about to park" usually is
not.

## ffs gaps found and fixed

The six tokio failures were all in `tokio::process`, and all were
differences between the emulated process tree and what the kernel shows
a tracee.  ffs makes every forked process a child of itself
(`CLONE_PARENT`) and reaps it as soon as it exits, emulating `wait4`,
`waitid`, `SIGCHLD` and `getppid` from a virtual tree so that checkpoints
copy it.  Three consequences were wrong (`src/proc.rs`), and a fourth gap turned up later:

1. **A reaped zombie's pid vanished.**  Natively a child that exited but
   was not waited for keeps its pid: `kill(pid, sig)` returns 0 and
   `pidfd_open(pid)` succeeds (tokio's `PidfdReaper::new` after a fast
   `true` child; `Child::kill` after `wait`).  Under ffs the real process
   was already gone (`ESRCH`).  Now `kill` of a virtual zombie returns 0
   and `pidfd_open` of one is rewritten to `eventfd2(1)`, a descriptor
   that polls readable at once, which is what the pidfd of a zombie
   reports (reads and `waitid(P_PIDFD)` on the stand-in are not
   emulated; tokio only polls it).
2. **`waitpid(WNOHANG)` could miss a child whose pidfd had just become
   readable** (`pidfd_reaper` polls, then `try_wait`).  The kernel makes
   the pidfd readable in the child's exit path, a moment before the
   tracer collects the exit status; the emulated wait answered from
   stale bookkeeping.  Now a wait or `pidfd_open` first drains pending
   wait statuses, waits (bounded, 500 ms) for children that are past
   their exit event, and recomputes the virtual zombies.
3. **SIGKILL of a process parked in an emulated `wait4`** (tokio's
   `kill_on_drop` test kills a `bash` waiting for `sleep`) made ffs
   inject an `rt_sigaction` query into a task that was dying, and the
   whole session failed with `task N died during remote syscall`.  A
   pending SIGKILL now just ends the virtual block, and a tracee dying
   during any injected syscall is a typed error (`remote::TaskDied`) that
   the callers acting on the tracee's behalf treat as "gone".

4. **(found by the 5000-schedule run) A process exiting while a thread
   slept in a virtual block** (tokio's `shutdown_timeout` tests leak a
   `spawn_blocking` thread in `thread::sleep(10_000 s)`).  After the main
   thread's `exit_group`, the scheduler found nothing runnable and jumped
   the virtual clock to the sleeper's deadline before the kernel's SIGKILL
   of that thread was observed, then reported a hang past `--max-vtime`;
   in other branches it wrote registers to the thread after it had died
   (`PTRACE_SETREGS ... No such process`) and the session failed.  Now the
   clock is not advanced once the main process's leader has exited (its
   threads are dying, not sleeping), no hang verdict is given while it
   finishes, and a register write to a task that vanished records the
   death instead (`Session::task_vanished`).  Seen 3 times in ~3000
   schedules of those tests, load-dependent; the saved schedule replays
   the hang deterministically and now replays to a clean exit.

5. **(found by libuv's test runner) `explore` stalled before its first
   branch** on a binary with megabytes of static buffers: the default
   watchpoint sampling expanded every writable global into 8-byte slots
   with a quadratic duplicate check.  It now draws a random global, then
   a random slot inside it (a large buffer counts once, which also stops
   big arrays from dominating the sample).

`tests/c/{zombie_pid,pidfd_wait,kill_waiter,exit_while_sleeping}.c` are the reproducers;
`process_tree_agrees_with_the_kernel` in `tests/integration.rs` explores
each 20 times.  Before the fix `pidfd_wait` failed in 20/20 schedules.

## What this says about using ffs on real software

* It works unmodified on the test suites of crates that use every Linux
  facility tokio does; the harness is the crate's own libtest binary.
* The cost is wall time, not porting: a schedule of a small test is tens
  of milliseconds, a stress test of 100k lock-free operations is minutes.
  Suites of this size want `-j`, and stress tests want fewer iterations
  or a checkpoint placed after setup.
* A sweep with 20 random/PCT schedules per test is a smoke test, not a
  proof: the two reproduced bugs needed 1000 schedules for a stable rate
  (and one is easier to hit natively).  Targeted harnesses with
  `ffs_checkpoint` after setup and `--watch`/`--watch-hold` on the
  contended word, as in `tests/liquid-cache`, are what found bugs before.
