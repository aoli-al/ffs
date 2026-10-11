# The gap between ffs and rr, measured with rr's test suite

Run on 2026-10-11: AMD Zen 5, Linux 6.18, glibc 2.44, rr commit 858a9fb
(Oct 2026).  566 of rr's tests were migrated (`tests.tsv`: all x86/generic
`BASIC_TESTS` plus the `TESTS_WITH_PROGRAM` whose driver is record + replay
+ token).  Every test is recorded under `ffs record` with the default
options (virtual time, virtual randomness, private pid namespace, policy
`mix` with preemptions), replayed, and replayed again with `--strict`.
The per-test status is in `baseline.tsv`; `summarize.py` groups a results
file by reason.

| status | before | after | meaning |
|---|---|---|---|
| PASS | 393 | 455 | token printed, replay identical, strict replay identical |
| REC_NOTOKEN | 91 | 75 | the program's own checks failed under ffs |
| REC_TIMEOUT | 22 | 5 | recording hung or was too slow (120 s) |
| REC_DEADLOCK | 20 | 5 | ffs reported every thread blocked |
| REPLAY_DIVERGED | 17 | 9 | replayed events differ from the trace |
| STRICT_DIVERGED | 5 | 7 | only a syscall return value differed |
| REPLAY_TIMEOUT / REPLAY_ERROR | 4 / 5 | 4 / 0 | replay hung, or ffs failed |
| OUTPUT_MISMATCH | 4 | 4 | schedule followed, program printed something else |
| REC_ERROR | 2 | 0 | ffs itself died while recording |
| BUILD_FAIL | 2 | 2 | `constructor`, `exit_fast` need rr's build rules |

"Before" is the state when the suite was first run; "after" is after the
work below.  455 of 564 buildable tests pass (81%).  Syscall throughput
went from 7,400 to 60,000 per second.  Three results are load-sensitive
(`clone_immediate_exit`, `grandchild_threads`, `x87env` pass alone but can
time out or diverge with 12 tests in parallel): the real-time grace that
classifies a thread as blocked, and the real-time slice cap, both depend on
machine load during recording.

## Shadow execution with output verification (2026-10-11)

The original table above is a historical baseline. The current replay
contract executes live syscalls, checks scalar returns by default and compares
captured buffers when `--verify-outputs` is enabled. External-state reset is
the test author's responsibility; recorded input injection has been removed.

Full run, eight parallel tests, using:

```
nix-shell --run 'cargo build --release && python3 tests/rr/run.py --verify-outputs --out /tmp/ffs-shadow-verify-rr -j 8'
```

Per-test results are in `verify-outputs.tsv`. Both replay passes used the
flag; `--strict` on the second pass is now a compatibility alias.

| status | count |
|---|---:|
| PASS | 435 |
| REC_NOTOKEN | 75 |
| REPLAY_DIVERGED | 35 |
| REC_TIMEOUT | 6 |
| REC_DEADLOCK | 5 |
| REPLAY_TIMEOUT | 3 |
| BUILD_FAIL | 2 |
| REC_ERROR | 2 |
| STRICT_DIVERGED (second replay) | 2 |
| OUTPUT_MISMATCH | 1 |
| Total | 566 |

**435/564 buildable tests pass (77.1%).** Of the 37 divergences across the
first and second replay passes, 20 have equal scalar return values but
different captured output bytes/addresses; 17 have other divergences. The
buffer-only cases primarily involve stat-family metadata, statfs and sysinfo.
They are observations of live-state differences, not automatically evidence
of a scheduling or virtualization defect. The old 455/564 baseline did not
check output buffers and is not directly comparable.

The two recording errors explicitly reject compat syscall ABIs. The remaining
recording failures include unsupported rr features, refused facilities,
deadlocks and timeouts; they are not failures of output comparison.
`dlchecksum` still has a stdout mismatch despite the captured checks passing,
which demonstrates that output-buffer coverage remains incomplete.

All 25 existing ffs integration tests passed after the shadow-execution
change. A new 26th regression also passed with `--verify-outputs`, checking
virtual clock buffers and stable pid/tid returns across a virtual sleep.
The live-input regressions check both default scalar validation and opt-in
buffer validation against files changed or removed between record and replay.

### Framework corrections after the output-verification baseline

The follow-up fixes address framework behavior without weakening replay
checks or substituting recorded syscall outputs:

* Preserve application `CLONE_PARENT` semantics in the logical process tree
  (`clone_parent`).
* Keep CPU-online redirection working with Nix's long `TMPDIR`, using an
  exclusively created short pathname with a session/checkpoint lifetime
  (`sysconf`, `sysconf_onln`).
* Separate application `PR_GET_TSC`/`PR_SET_TSC` state from the tracer's
  internal TSC trap, including inheritance and checkpoints (`prctl`,
  `prctl_tsc`).
* Execute synchronous `process_vm_writev` normally, and capture
  `process_vm_readv` output for verification (`vm_readv_writev`).
* Resume already-consumed ptrace exit stops during cleanup
  (`futex_exit_race_sigsegv`, `grandchild_threads_main_running`,
  `grandchild_threads_thread_running`).
* Return a virtual timeout instead of leaking an internal interruption from
  `epoll_wait`; restart waits interrupted by a late preemption signal without
  resetting their deadline (`epoll_many`). Correct the `ppoll` timeout and
  `epoll_pwait2` signal-mask argument positions, and update raw `ppoll` and
  `pselect6` remaining-time buffers.

The native regression fixture `tests/c/kernel_semantics.c` covers these
syscall semantics, including a forced internal interruption, with live
record/replay and `--verify-outputs`. All 27 integration tests pass.
`epoll_many` also passed 30 consecutive record/replay/second-replay runs with
output verification while the full suite ran concurrently.

The final full run used the same command above with
`--out /tmp/ffs-fixed-verify-rr -j 8`; its complete results are saved in
`verify-outputs-fixed.tsv`. **443/564 buildable tests pass (78.5%)**, compared
with 435/564 in the preceding output-verification baseline. All ten previously
failing tests named in the fixes above pass in this run.

| status | count |
|---|---:|
| PASS | 443 |
| REC_NOTOKEN | 68 |
| REPLAY_DIVERGED | 38 |
| REC_DEADLOCK | 5 |
| REC_TIMEOUT | 4 |
| BUILD_FAIL | 2 |
| REC_ERROR | 2 |
| STRICT_TIMEOUT | 1 |
| OUTPUT_MISMATCH | 1 |
| STRICT_DIVERGED | 1 |
| REPLAY_TIMEOUT | 1 |
| Total | 566 |

The aggregate increase is smaller than the targeted improvement because
five baseline passes failed in the full rerun: `alarm`,
`pid_ns_kill_threads_exit_wait`, `pid_ns_segv`, `sigtrap`, and
`async_kill_with_syscallbuf`. These are not counted as resolved or silently
replaced with results from another run.
An isolated follow-up rerun passed `alarm` and `pid_ns_segv`; the other three
failed again (`pid_ns_kill_threads_exit_wait`: recording timeout, `sigtrap`:
missing success token, `async_kill_with_syscallbuf`: replay divergence).
Their cause relative to the baseline remains unresolved.

Other ffs gaps remain, including `waitid(P_PIDFD)`, signal-mask preservation
around TSC faults, and asynchronous process-exit replay. Tests that assume rr's
syscall-buffer patches or rr-specific options are separate compatibility
requirements; for example, `user_ignore_sig` explicitly requires rr's
signal-suppression option. Live metadata differences remain errors when
output verification is requested.

## What was fixed

Each item names the tests it turned green.

* **Environment and cwd recorded in the trace** and set by `ffs replay`;
  the dynamic loader parses the environment, so it changes tick counts.
  (Every replay of a trace made in a different shell.)
* **Tracer-generated signals in replay.**  Virtual timer signals and emulated
  `SIGCHLD` were discarded as "external" during replay (`pause`, `ppoll`,
  `intr_*`, `multiple_pending_signals*`, `two_signals_with_mask`, 9 tests).
* **Signal delivered by the syscall being entered** (a `ppoll` sigmask,
  `read` with a signal pending): `deliver_signal_loop` now enters the
  syscall properly and keeps going over the restart exit stop.
* **Preemption signal and emulated sleeps.**  A pending `SIGSTKFLT`, or an
  ignored signal (which the kernel queues for a traced task), turned a
  virtual `nanosleep` into `EINTR` (`nanosleep`, `threads`, `intr_sleep`,
  `futex_priorities`, 7 tests).
* **`SIGPIPE` and the other dispositions of ffs** were inherited by the
  program; every disposition is reset before exec (`signal_deferred`).
* **The program could block ffs's preemption signal**; it is now stripped
  from every mask set by `rt_sigprocmask`, `rt_sigsuspend`, `ppoll`,
  `pselect6`, `epoll_pwait*` (`no_mask_timeslice`, `sigprocmask_evil`).
* **Loops without conditional branches** never reached a tick budget; a
  real-time slice cap (`--slice-cap-ms`) interrupts them, recorded as a
  `Slice` event that also advances the virtual clock (`grandchild_threads*`,
  `clone_immediate_exit`).
* **`vfork` / `CLONE_VFORK`**: the parent blocks after the clone event, and
  the child gets its argument registers back (glibc's `vfork` keeps the
  return address in `rdi` across the syscall) (`vfork_*`, `clone_vfork*`,
  `64bit_child`, `exec_shared_as`, 7 tests).
* **`exec` invalidates `/proc/<pid>/mem`**: the handle is reopened after
  exec (`futex_restart_clone`; breakpoints after an exec went nowhere).
* **External `SIGKILL`**: a thread's `Exit` with no exit syscall, fatal
  signal, kill-family syscall or seccomp filter before it is replayed by
  sending `SIGKILL` at the recorded point (`async_kill_with_*`).
* **Job control** emulated: default-action stop signals stop the process
  (not scheduled until `SIGCONT`/`SIGKILL` is pending), `WUNTRACED`,
  `WCONTINUED` and `SIGCHLD` with `CLD_STOPPED`/`CLD_CONTINUED`
  (`sigstop`, `wait_sigstop`, `sigcont_threaded`,
  `group_stop_thundering_herd`, `pidfd_getfd`).
* **`PR_SET_PDEATHSIG`** delivered by the emulated process tree
  (`prctl_deathsig`).
* **A fault in the init of a pid namespace** the program created: the kernel
  ignores a tracer-injected signal there, so it is killed by ffs and its
  parent sees the real status (`pid_ns_segv`); and the first fork after
  `unshare(CLONE_NEWPID)` must not get `CLONE_PARENT` (`pid_ns_kill_*`).
* **`/proc` for the pid namespace**: the tracee gets a mount namespace with
  its own procfs, so `/proc/<getpid()>/...` works; descriptors ffs hands to
  checkpoint copies now travel over `SCM_RIGHTS` instead of
  `/proc/<ffs>/fd`, and ffs keeps the capabilities of its user namespace
  (`pid_ns_*`, `mount_ns_*`, `netfilter*`, `bpf_*`, `tun`, 17 tests).
* **Tracer signals** as in rr: `kill(0, sig)` from the program no longer
  kills ffs; `SIGTERM` ends the run cleanly (`abort`, `sigtrap_process_group`,
  `abort_nonmain`, `term_nonmain`).
* **A run ends when the main process exits**, remaining daemons are killed,
  as `rr record` does (`daemon_read`, `exec_stopsig`, `shm`).
* **Virtualized** `adjtimex`, `clock_adjtime`, `times`, `getrusage`,
  `sched_getaffinity` and `/sys/devices/system/cpu/online` (one cpu), and
  `select` writes the remaining virtual time back (`adjtimex`,
  `clock_adjtime`, `times`, `sysconf`, `sysconf_onln`).
* **Recorded return values** are the ones the program sees after
  virtualization, so `--strict` no longer trips on `times` or `time`.
* **Replay robustness**: a blocked syscall gets the deadlock timeout rather
  than the 100 µs grace (disk latency under load reclassified `write` as
  blocked), breakpoints in not-yet-mapped code fall back to single steps,
  unreadable rips, threads that die without an exit stop, `Blocked` steps of
  a restarted syscall, and trailing exit steps of a killed process.
* **Throughput**: the wait loop sleeps on `SIGCHLD` with `sigtimedwait`
  instead of polling `/proc` with exponential backoff: 7,400 to 60,000
  syscalls per second (`hooks`, `rdtsc_loop*`, `alarm`, `check_lost_*`).

## What remains, by cause

### Out of scope: rr features (about 63)

* **The tracee uses ptrace** (37): `ptrace*`, `ptracer_death*`,
  `kill_ptracee`, `vfork_done`, `vfork_setopts`, `intr_ptrace_decline`.
  Not needed for concurrency testing; emulation would reuse the process
  tree machinery if ever wanted.
* **rr's syscall buffer and code patching** (9): `patch_*`, `syscallbuf_*`,
  `clone_syscallbuf_cleanup_*`.
* **rr command-line options or magic** (10): `user_ignore_sig`, `int3_ok`,
  `chaos_oom`, `exclusion_region`, `strict_priorities`,
  `prctl_syscall_user_dispatch`, `gcrypt_rdrand`, `signal_frame`,
  `redzone_integrity`, `prctl_tsc`.
* **rr refuses a syscall** (7): `aio`, `io_uring`, `userfaultfd`,
  `perf_event_mmap`, `futex_pi`, `madvise_free`, `nscd`, `close_range`,
  `kcmp`.  The first four should be refused by ffs too once memory effects
  are logged (they write tracee memory asynchronously).

### ffs limitations (about 40)

* **Live kernel state** (16), the gap `docs/replay-design.md` closes with
  the input log: kernel ids (`keyctl`, `msg`, `sem`, `shm`, `shm_unmap`),
  files left by the recording (`openat2`, `mmap_recycle`, `chmod`),
  `statfs`, `readdir`, `quotactl`, `fcntl_notify`, `perf_event`,
  `async_signal_jit`, `block` (socket buffer state), `dlchecksum`
  (writable library data differs between replays; not located), `lsl`.
* **Nested pid namespaces** (3): the init's exit should kill the namespace
  (`pid_ns_kill_threads`, `pid_ns_kill_threads_exit_wait`,
  `pid_ns_shutdown`); `mount_ns_exec*` (3) diverge on replay after `mount`.
* **Real-time tests** (4): `async_kill_with_threads_*` are killed from
  outside after N real seconds while the program's progress is virtual;
  `timer` waits 5000 times for a 100 ms timer; `timerfd` is not virtualized.
* **Replay of long busy loops** is slow: the overflow interrupt skid bound
  is 10,000 ticks (rr uses the same on Zen; 2,000 was tried and overshot
  under load), and every tick of that window is replayed by breakpoint or
  single step (`check_lost_interrupts` replay times out).
* **Not supported**: `exec` from a non-leader thread
  (`exec_from_other_thread`), 32-bit syscalls via `int 0x80`
  (`cross_arch`), a program that needs a tty (`tty_tostop`), `/bin/ls`
  (`checksum_sanity`, `mremap_after_coalesce`: this machine has none).
* **Unclassified** (8): `epoll_many`, `fault_in_code_page`, `pidfd`,
  `prctl` (`PR_GET_TSC` should report enabled), `sched_yield` (expects
  strict alternation), `seccomp_signals`, `sigprocmask_exec`,
  `stack_growth_with_guard`, `tracee_unmap_vdso`,
  `unexpected_exit_execve_twice`, `sigprocmask_ensure_delivery`.

To rerun only the failures:

```
python3 tests/rr/run.py -j 8 --only $(awk -F'\t' '$2!="PASS"{print $1}' tests/rr/baseline.tsv | paste -sd,)
```
