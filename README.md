# ffs — fork-based deterministic concurrency testing

`ffs` is a Linux/x86-64 concurrency testing tool written in Rust. It runs an
unmodified multithreaded program under `ptrace`, serializes its threads onto a
deterministic scheduler (one tracee thread runs at a time), records the
schedule, and checks replay against it. Its distinguishing feature, and the reason it
exists, is **checkpointing**: at any scheduling point the whole process — every
thread, and every process the program forked — can be cloned with `fork`, and
alternative schedules can then be explored from that point without re-running
the program from the beginning.

The mechanisms follow [rr](https://rr-project.org): hardware "ticks"
(retired conditional branches counted by a per-thread perf counter) measure a
thread's progress deterministically and let the tracer stop a thread at an exact
instruction; syscalls are intercepted at ptrace syscall stops; checkpoints are
made by injecting `clone`/`fork` syscalls into the stopped tracee the way rr's
`AutoRemoteSyscalls`/`Task::os_fork_into` do. `ffs` keeps live kernel objects so checkpoint branches can run
new schedules. Replay is **shadow execution**: ordinary syscalls execute
again and their return values must match the recording. The same time,
randomness and process-identity virtualization applies during recording and
replay. The user is responsible for controlling external inputs and resetting
file or network side effects before replay.

## Building

Dependencies are managed with Nix:

```
nix develop            # or: direnv allow
cargo build --release
make -C tests/c        # example/test programs -> tests/bin/
cargo test             # integration tests (need ptrace + perf counters)
```

`ffs check` verifies that the hardware tick counter works on the machine
(`perf_event_paranoid` must be ≤ 2 or the user needs `CAP_PERFMON`). Unprivileged
user namespaces must be enabled for stable pids (see below); without them
`ffs` still works but warns.

## Usage

```
ffs record  [opts] -o trace.json -- ./prog args...   # one run, random schedule
ffs replay  trace.json                                # deterministic re-execution
ffs explore [opts] -- ./prog args...                  # many schedules from a checkpoint
ffs show    trace.json                                # print a schedule
ffs check                                             # PMU self-test
```

### The tracee-side API (`include/ffs.h`)

The header is optional and has no runtime dependency: every call is a raw
syscall with a number Linux does not implement, so natively it returns
`-ENOSYS` and does nothing. Under `ffs` the tracer intercepts it.

| call | meaning |
|---|---|
| `ffs_checkpoint("name")` | take a checkpoint here; `ffs explore` roots its search at it |
| `ffs_begin()` / `ffs_end()` | mark the region of interest (also a valid root; after `ffs_end()` branches finish with a fixed schedule) |
| `ffs_assert(cond, "msg")` | report a property violation; the failing schedule is saved |
| `ffs_yield()` | extra scheduling point |
| `ffs_log("msg")` | print through the tracer |
| `ffs_is_active()` | 1 under ffs |

Programs that do not use the header are explored from their start; crashes
(`abort()` from a failed `assert`, segfaults), deadlocks and non-zero exits
count as failures.

### Exploring

```
$ ffs explore --iterations 300 -- tests/bin/bootstrap
[ffs] reached exploration root after 102 steps: ffs_checkpoint("after-init")
[ffs] root checkpoint taken (1 threads); strategy=random
[ffs] branch 13: FAILURE assertion failed: lost update after 59 steps; schedule saved to ffs-out/fail-1.json
[ffs]   re-execution from the checkpoint: reproduced
```

The program's expensive bootstrap (50M iterations in this example) runs once;
every branch is a fork of the root checkpoint and takes milliseconds. A failing
branch is saved as a trace (bootstrap prefix + branch) and immediately
re-executed from the checkpoint to confirm it reproduces. The saved trace can
also be replayed from scratch later:

```
$ ffs replay ffs-out/fail-1.json
[ffs] replayed 162 of 162 steps
[ffs] ASSERTION FAILED: lost update
```

Strategies (`--strategy`):

* `random` (default): independent walks from the root, each with a fresh
  seed of the chosen policy. With `--policy random` the thread choice is
  random at every event; with `--policy pct` it is PCT (Burckhardt et al.):
  random thread priorities, the highest-priority runnable thread always runs,
  and `--pct-depth - 1` randomly placed priority change points, which finds a
  bug of depth *d* with a guaranteed probability per run.
* `dfs`: depth-first search over the tree of thread choices. Every node with
  unexplored alternatives keeps a forked checkpoint (up to
  `--max-checkpoints`), so backtracking is a fork; beyond the budget the path is
  re-executed from the nearest checkpoint (and verified to be deterministic).

Scheduling points are syscalls, `ffs_yield()`, and three kinds of
preemption inside user code:

* **tick budgets**: with probability `--preempt-prob` a slice gets a
  log-uniform budget of up to `--max-budget` retired conditional branches;
* **instruction steps**: after a budget expires the thread is single-stepped
  up to `--max-insn-steps` more instructions, so a preemption can land
  between any two instructions, not only at a branch;
* **watchpoints**: every access to a watched 8-byte slot traps (x86 debug
  registers, 4 slots). `--watch a,b,buf+16,0x4c5e08` names globals of the
  executable; by default `explore` samples `--watch-count` writable globals
  from the symbol table for each branch. This is what makes races between
  two straight-line stores (`a = 1; b = -1;`) reachable: counting branches
  can never stop there, a watchpoint on `a` always does.

All three are recorded as ordinary `Preempt` steps at `(ticks, rip)`, so a
replay needs none of them.

Other useful options: `--root start|begin|checkpoint|auto`, `--keep-going`,
`-v` (print every scheduling step), `--ticks insns|none` (fallback tick
sources: instruction counter, or no preemption at all), and the opt-outs
`--real-time`, `--real-random`, `--no-pidns` described below.

## What is deterministic

ffs controls many sources of nondeterminism, but does not guarantee identical
runs from the same seed or bit-exact replay for arbitrary Linux programs.
A saved schedule controls interleaving more precisely than a seed alone.
The following facilities are controlled:

* **Thread interleaving.** Exactly one thread runs at a time; switches happen
  at syscalls, at tick-counted preemptions (exact to the instruction), at
  signal deliveries and at the `ffs_*` calls.
* **Time** (`--real-time` to disable). `clock_gettime`, `gettimeofday`,
  `time`, the vDSO fast paths and `rdtsc`/`rdtscp` report a virtual clock that
  advances with the ticks the program executes. Everything that *waits* for
  time is driven by that clock, like a discrete-event simulation:
  `nanosleep`/`sleep`/`usleep` complete when the virtual clock reaches their
  deadline (instantly in real time if nothing else can run), timeouts of
  `futex` (`pthread_cond_timedwait`, `sem_timedwait`, `pthread_mutex_timedlock`),
  `poll`/`ppoll`, `select`/`pselect6`, `epoll_wait`/`epoll_pwait[2]`,
  `rt_sigtimedwait` and `futex_waitv` expire on the virtual clock, and
  `setitimer`/`alarm`/`timer_create` timers fire at a deterministic tick with
  the `siginfo` the kernel would have produced. `getcpu` reports cpu 0 and
  `rseq` is reported as unsupported, so no real cpu number leaks in.
* **Randomness** (`--real-random` to disable). `getrandom`, reads from
  `/dev/urandom` and `/dev/random`, and the `AT_RANDOM` bytes the kernel puts
  on the stack at `execve` (glibc's stack canary and pointer guard) come from a
  ChaCha stream seeded from `--seed` and recorded in the trace.
* **Process and thread ids** (`--no-pidns` to disable). Tracees run in a
  private pid namespace in which `ffs` chooses every pid, so a thread has the
  same `gettid()`/`getpid()` in every branch of an exploration and in a cold
  replay. This is what keeps glibc's recursive and error-checking mutexes,
  `pthread_self()`, `pthread_kill()`, robust lists and the process tree
  consistent across a checkpoint. The tracee gets a mount namespace with its
  own `/proc`, so `/proc/<getpid()>/...` works; descriptors ffs hands to a
  checkpoint copy travel over a Unix socket (`SCM_RIGHTS`). The program sees
  one cpu: `getcpu`, `sched_getaffinity` and `/sys/devices/system/cpu/online`
  say so.
* **The process tree.** `fork`/`vfork`/`clone`/`clone3`-created processes are
  made children of `ffs` (with `CLONE_PARENT`), and `wait4`, `waitid`,
  `getppid` and the `SIGCHLD` of an exiting child are emulated by the tracer
  from a tree it keeps in logical ids, the way rr replays them from its
  recording. A child forked before a checkpoint is therefore still its parent's
  child in every branch; waits block and wake under the scheduler's control.
* **Asynchronous signals.** A signal is a scheduling event recorded with its
  full `siginfo` at an exact (thread, ticks, instruction). On replay it is made
  to arrive at that point: an already pending signal is let through, a
  synchronous fault is reproduced by executing the faulting instruction, and
  anything else is injected with `tgkill` and its `siginfo` restored, also
  when it interrupts a blocking syscall. Signals from outside the session
  that the trace does not expect (a stray `SIGINT`, a real timer still armed
  under `--real-time`) are discarded during replay; a `SIGKILL` from outside
  is recorded by the thread's exit and re-sent at the same point. The
  program cannot block ffs's preemption signal: it is removed from every
  mask the program installs, as rr does.
* **Job control.** `SIGSTOP`, `SIGTSTP`, `SIGTTIN` and `SIGTTOU` with the
  default action stop the process in emulation: it is not scheduled until a
  `SIGCONT` (or `SIGKILL`) is pending for it, and `wait4`/`waitid` with
  `WUNTRACED`/`WCONTINUED` and the parent's `SIGCHLD` report it. A fault in
  the init of a pid namespace the program created (which the kernel would not
  let a tracer's injected signal kill) terminates it with the status the
  parent expects, and `PR_SET_PDEATHSIG` is delivered when the emulated parent
  exits.
* **The environment.** The environment and working directory of the
  recorded run are stored in the trace and used by `ffs replay`: the dynamic
  loader and libc parse them, so they change tick counts.
* **Loops without conditional branches** (`for(;;);`) never reach a tick
  budget; after `--slice-cap-ms` of real time such a thread is interrupted
  and that is recorded as a `Slice` preemption, which also advances the
  virtual clock by the cap. A replay reaches it by breakpoint like any
  preemption.

* **Files the program writes** (`--no-isolate-files` to disable). During an
  exploration, a file that a branch creates, truncates, renames or unlinks
  is snapshotted before the syscall and put back when the branch ends, and
  files already open for writing at the root are snapshotted at the start
  of every branch. Every branch therefore starts from the same files, and
  the working directory is left as the exploration found it.

### Shadow replay

New traces use version 2. Replay checks syscall return values by default,
rejects incomplete execution and terminal-outcome mismatches. With
`--verify-outputs`, it also compares captured syscall buffers before resuming
another thread. `--strict` remains a
compatibility alias; `--lenient` disables return-value checks only for legacy
version 1 traces, which do not contain recorded buffers.

Captured successful outputs currently cover native x86-64 `read`, `pread64`, `readv`,
`preadv`, `preadv2`, `process_vm_readv`, `getdents`, `getdents64`, `getrandom`, `stat`/`fstat`/
`lstat`/`newfstatat`/`statx`, `readlink`/`readlinkat`, `uname`, `sysinfo`,
`statfs`/`fstatfs`, `gettimeofday`, `time`, `clock_gettime`, and `clock_getres`.
Buffers are stored inline in JSON, limited to 64 MiB per captured syscall.
Compat/i386 and x32 syscall entries are rejected before applying the native
ABI output descriptions; syscall inspection requires Linux 5.3 or newer.
Unsupported syscalls and partial memory writes from failed syscalls have no
buffer verification.

Reads, metadata queries, directory enumeration and writes execute against
live kernel state. Recorded values never replace their results, and replay
does not restore fd offsets from the trace. A different return value, including
an errno difference, is a replay divergence (exit code 5). Values are compared
**after** ffs's normal virtualization, so clocks and process identities use
the same model in record and replay.

Output-buffer verification is optional because metadata can contain incidental
kernel state such as inode numbers for a newly created stdout pipe. It also
catches changes that scalar returns cannot: `read` can return eight different
bytes while still returning 8. Even with the option enabled, buffer coverage
is partial, as listed above.

The user must arrange repeatable inputs and avoid, isolate or reset external
side effects. Existing exploration file snapshots are a convenience for the
operations they cover, not a replacement for this responsibility. A passing
shadow replay verifies recorded observations; it is not an archive of the
original machine's state.

Version 2 also records the tick source, slice-cap duration and use of a PID
namespace. Replay restores those settings and rejects a mismatched PMU or an
unavailable required PID namespace.

### Remaining nondeterminism

The seed does not determine host-timed slice preemptions, hardware overflow
interrupt skid, external signals, or kernel readiness and wake ordering.
Synchronous regular-file I/O and known synchronous queries no longer become
`Blocked` scheduling events merely because the disk or host is slow.
`--slice-cap-ms 0` disables wall-clock slice preemption, but branchless loops
may then run forever; this alone does not make same-seed runs deterministic.

Network and terminal input, external pipes, many ioctls, kernel-generated
object ids and file-backed mappings still depend on live state. Successful
replay means the recorded checks passed, not that every byte of process or
kernel state was identical.

Asynchronous-memory interfaces (`io_setup`, `io_submit`, `io_uring_*`,
`userfaultfd`, `perf_event_open`) return `ENOSYS` under
ffs. `timerfd_*` also return `ENOSYS` with virtual time, until their counters
and readiness can be virtualized; `--real-time` allows native timerfds.
Inherited asynchronous resources and direct hardware randomness are not
covered by these syscall guards.

## How it works

### Execution model (`session.rs`, `task.rs`)

The tracee is spawned with `PTRACE_SEIZE` and the options
`TRACESYSGOOD|TRACECLONE|TRACEFORK|TRACEVFORK|TRACEEXEC|TRACEEXIT|EXITKILL`,
with `ADDR_NO_RANDOMIZE` and (with virtual time) `PR_SET_TSC=SIGSEGV`.
Exactly one thread is resumed at a time (`PTRACE_SYSCALL`), and it runs until
one of these **events**, which are the scheduler's decision points:

* `SyscallEntry` — before the syscall executes (a thread may be parked here);
* `SyscallExit` — after it; clone/fork events are folded into it;
* `Blocked` — the thread entered a syscall and blocked in the kernel
  (detected via `/proc/<tid>/stat`), or was parked by the tracer (an emulated
  sleep or wait), so the scheduler switches away; it becomes runnable again
  when its exit stop is observed or its virtual deadline passes;
* `Preempt` — its tick budget expired (perf-counter overflow delivered as
  `SIGSTKFLT` to the thread, intercepted and suppressed), or a virtual
  deadline fell inside the slice;
* `Signal`, `Exit`, `Exited`, and the `ffs_*` magic calls.

Each step of a schedule is `(thread, event, ticks, rip)`. Threads have stable
logical ids (creation order) so traces are comparable across forks. A
replayed trace is checked event by event; the first difference is reported as
a divergence.

### Ticks and exact replay (`perf.rs`, `Session::run_task_to`)

Per thread, a pinned user-only perf counter counts retired conditional branches
(`0x5100d1` on AMD Zen, `0x51{01,11}c4` on Intel — rr's PMU table).
Overflow is routed to the thread as `SIGSTKFLT` (`F_SETOWN_EX`/`F_SETSIG`).
To replay a preemption recorded at `(ticks, rip)`, the counter is programmed to
interrupt `skid` ticks early, then an `int3` breakpoint on `rip` (or single
stepping, when already at `rip`) advances the thread until its tick count is
exactly right. The count is deterministic on this hardware (verified: identical
to the tick across runs, including with `lock`-prefixed atomics).

A subtle race is handled explicitly: a budget is never armed when resuming
*into* a syscall, and a syscall aborted with `ERESTARTSYS` by a late overflow
interrupt is restarted transparently, so the trace never depends on interrupt
skid.

### Virtual time (`vtime.rs`, `vdso.rs`)

The vDSO entry points for `clock_gettime`, `gettimeofday`, `time`,
`clock_getres` and `getcpu` are patched in the tracee's private mapping into
`mov $nr,%eax; syscall; ret`, so they become real syscalls that the tracer
sees; their results, and trapped `rdtsc`/`rdtscp`, are replaced by a clock
derived from the total number of ticks executed (plus a small constant per
scheduler event, so that time moves for threads that only make syscalls).

Waiting is virtualized in two ways. `nanosleep`/`clock_nanosleep`,
`wait4`/`waitid` and the timer syscalls are not executed at all: the syscall is
aborted at its entry stop and the tracer supplies the result at the exit stop,
possibly after parking the thread ("virtual block") until its deadline or a
pending signal. Blocking syscalls with timeouts are executed, but with the
timeout stripped; the tracer remembers the virtual deadline and, when the
clock passes it while the thread is still blocked, interrupts the syscall
(`PTRACE_INTERRUPT`) and sets the timeout result. When no thread can run and
a deadline is pending, the clock jumps to the earliest one. All of this is a
function of the schedule, so a replay makes the same decisions.

### Checkpoints (`checkpoint.rs`, `remote.rs`, `fdpass.rs`, `ns.rs`)

`Session::fork_session` clones every process of the session:

1. `normalize_all`: blocked threads get `PTRACE_INTERRUPT`ed and rewound to
   re-execute their syscall; threads at a syscall entry have the syscall aborted
   (`orig_rax = -1`) and are rewound too. The same normalization is recorded as
   a `Fork` step so replays stay in sync.
2. For every thread: registers, XSAVE state, signal mask, `sigaltstack`,
   robust-futex list, `clear_child_tid` address, name, rseq registration,
   pending signals and tracer-side state (virtual block, deadline, emulated
   wait) are captured (some via injected syscalls).
3. One thread injects `clone(CLONE_PARENT|SIGCHLD)`: a fork whose child is
   traced automatically and whose parent is `ffs` itself. The pid namespace
   is steered (`ns_last_pid`, as CRIU does) so that a live copy gets the
   canonical pid `10000 + logical id` of the original, and a frozen copy
   gets an id from a range nobody observes.
4. Inside the child, each other thread is recreated with
   `clone(CLONE_VM|CLONE_FS|CLONE_FILES|CLONE_SIGHAND|CLONE_THREAD|CLONE_SYSVSEM|CLONE_SETTLS|CLONE_CHILD_SETTID|CLONE_CHILD_CLEARTID)`
   (again with a steered tid) and the captured state is applied
   (`PTRACE_SETREGS` incl. `fs_base`, `NT_X86_XSTATE`, `PTRACE_SETSIGMASK`,
   `set_robust_list`, `sigaltstack`, `prctl(PR_SET_NAME)`, `rseq`). Pending
   signals are re-raised; the siginfo of signals the tracer itself sent
   (timers, `SIGCHLD`) travels with the session.
5. Kernel objects the copy would otherwise share with the original are
   replaced: regular files are re-opened at their offset; pipes whose both
   ends are in the session become new pipes with the buffered bytes copied
   (`tee`); eventfds are recreated with their counter; socketpairs created
   under `ffs` become new pairs with their buffered data (`MSG_PEEK`), handed
   to the tracee over a Unix socket with `SCM_RIGHTS`; writable `MAP_SHARED`
   mappings (anonymous, memfd, `/dev/shm`) are copied into memfds and
   remapped (rr's EmuFs). Objects shared between the processes of one
   checkpoint stay shared between their copies.

Remote syscalls use the `syscall` instruction inside the vDSO, save/restore the
registers around it, and pick up `PTRACE_EVENT_CLONE/FORK` stops for the new
tids — the same scheme as rr's `AutoRemoteSyscalls`.

The source session is untouched, so a checkpoint is just a session nobody
resumes; "resume from checkpoint" is `fork_session` on it again.

### Emulated process tree (`proc.rs`)

Because every tracee process is really a child of `ffs`, the tracer reaps all
of them and knows every exit status. It keeps `child -> parent` in logical
ids, a list of exited-but-not-waited-for children, and answers `wait4`/`waitid`
(all selectors, `WNOHANG`, `WNOWAIT`, `ECHILD`, `EINTR` vs. `SA_RESTART`),
`getppid` and `SIGCHLD` (with `CLD_EXITED`/`CLD_KILLED`/`CLD_DUMPED`,
`si_pid`, `si_status`) from it. Stopped/continued children are not reported
(tracees never stop).

## Java programs (`tests/java/`)

A JVM is an ordinary multithreaded tracee: `ffs` runs OpenJDK 17 unmodified,
including the JIT compiler threads, the garbage collector, safepoint polls and
implicit null checks (which HotSpot implements with `SIGSEGV`), with no
special support. `tests/java/Ffs.java` is a tiny JNI shim over `include/ffs.h`
so a harness can call `Ffs.begin()`, `Ffs.check(cond, msg)` and
`Ffs.yield()`; `tests/java/build.py` compiles the harnesses into
`tests/bin/java/{buggy,fixed}`.

Pass `-XX:-UsePerfData`. At startup HotSpot lists `/tmp/hsperfdata_<user>` to
clean stale files, so the directory's contents (left by killed branches or
other JVMs) are live input that makes a cold replay diverge on `getdents64`.

### FLINK-37663

`tests/java/flink/FutureCompletingBlockingQueue.java` is the queue from
Flink's connector base, verbatim. `wakeUpPuttingThread` signals a putter's
condition but leaves it in the `notFull` queue, so a later `signalNextPutter()`
wakes a thread that is no longer waiting and the putter that is waiting
stays stranded ([FLINK-37663](https://issues.apache.org/jira/browse/FLINK-37663)).
`Flink37663.java` sets up a queue of capacity 1 with two blocked producers,
calls `Ffs.begin()`, wakes producer A and polls once; the property is that
producer B completes. `build.py` also produces the `fixed` variant with the
upstream fix (remove the condition from `notFull` when the wait ends).

```
$ ffs explore --iterations 200 --keep-going -- java -XX:-UsePerfData \
    -Djava.library.path=tests/bin/java -cp tests/bin/java/buggy Flink37663
[ffs] reached exploration root after 6846 steps: ffs_begin()
[ffs] root checkpoint taken (16 threads); strategy=random
[ffs] branch 2: FAILURE assertion failed: FLINK-37663: producer B stranded after consumer freed a slot ...
[ffs]   re-execution from the checkpoint: reproduced
...
[ffs] explored 200 branches, 155 failing
$ ffs replay ffs-out/fail-1.json          # cold replay, from JVM start
[ffs] replayed all 12474 steps with recorded checks
[ffs] ASSERTION FAILED: FLINK-37663: producer B stranded after consumer freed a slot
```

| variant | policy | branches | failing |
|---|---|---:|---:|
| buggy | random (`mix`) | 200 | 155 |
| buggy | PCT | 100 | 100 |
| fixed | random | 200 | 0 |

Each branch is a fork of the JVM at the checkpoint and takes about 50 ms
(a failing one waits 10 virtual seconds for B, which is instant in real
time but lets the JVM's periodic tasks run). A plain `ffs record` of the
harness also finds the bug in roughly 40% of runs, and every recording
replays exactly (`replayed all N steps with recorded checks`). Natively the
harness passes.

Running the JVM exposed three bugs in ffs, all fixed:

* **`pushf`/`popf` under single-stepping.** HotSpot's generated code saves and
  restores the flags. A single-stepped `pushf` pushes the trap flag the step
  set, and the kernel stops attributing TF to the debugger across a `popf`
  (`is_setting_trap_flag`), so after the `popf` the program kept trapping on
  every instruction and died of `SIGTRAP`. ffs now clears TF in the pushed
  word after stepping a `pushf` and clears a trap flag that became visible
  after a step, as rr's `Task::did_waitpid` does.
* **Checkpoint copies of a joined thread.** glibc 2.44 makes a thread's
  `clear_child_tid` address its join-state word, not its tid. Recreating
  threads in a checkpoint with `CLONE_CHILD_SETTID` wrote the tid into that
  word, so a `pthread_join` in the copy got `EAGAIN` and the saved trace
  did not replay from a cold start. Copies now use `CLONE_CHILD_CLEARTID`
  only.
* **`restart_syscall` labels.** A timed `poll` interrupted by the preemption
  signal is restarted by the kernel through `restart_syscall`; its exit was
  recorded with number 219 instead of the original call's.

## Results on rr's test suite

`tests/rr/` runs rr's own test programs under `ffs record`/`ffs replay`;
`tests/rr/GAP.md` has the current numbers and what the remaining failures
are. Per-syscall cost is about 16 µs (60k syscalls/s) with the
SIGCHLD-driven wait loop.

## Results on SCTBench

[SCTBench](https://github.com/mc-imperial/sctbench) collects 49 pthread
programs with known concurrency bugs (CHESS, conc-bugs, the
concurrent-software-benchmarks suite, inspect, parsec, radbench, safestack,
splash2); 46 build on a current toolchain. None of them uses the `ffs.h` API;
each was explored from its start with `ffs explore --iterations 1000 --seed 1`
(default watchpoint sampling and instruction steps), once with
`--policy random` and once with `--policy pct`. The table gives the branch at
which the bug was first hit and how it manifested; every failure was
re-executed from the checkpoint and reproduced.

| program | random | pct |
|---|---|---|
| chess WorkStealQueue / Interlocked / InterlockedWithState / State | 16 / 162 / 742 / 497 (abort) | – |
| conc-bugs aget-bug2 | 327 (abort) | – |
| conc-bugs pbzip2-0.9.4 | – | 1 (segfault) |
| conc-bugs stringbuffer-jdk1.4 | 2 (segfault) | 2 |
| csb account, arithmetic_prog, bluetooth_driver, circular_buffer, fsbench, lazy01, queue, stack, twostage, wronglock, wronglock_3 (asserts) | –, 1, 12, 3, 1, 1, 2, 3, 109, 2, 2 | 164, 1, 204, 42, 1, 2, 204, 5, 28, 49, 73 |
| csb carter01, deadlock01, din_phil2–7, phase01, sync01, sync02 (deadlocks) | 12, 86, 1, 1, 1, 1 | 13, 35, 1, 1, 1, 1 |
| csb reorder_3 / reorder_4 / token_ring | 415 / – / – | 744 / 212 / 198 |
| csb reorder_5, reorder_10, reorder_20, twostage_100 | – | – |
| inspect ctrace-test (exit 6) / qsort_mt (abort) | 1 / 87 | 1 / – |
| parsec streamcluster (deadlock) / streamcluster3 (segfault) | 2 / 4 | 2 / 1 |
| radbench bug6 (deadlock) | 1 | 19 |
| splash2 barnes / fft / lu (asserts) | 7 / 1 / 4 | 2 / 1 / 2 |
| safestack bug1 | – | – |
| parsec streamcluster2 | – | – |

Together the two policies expose 40 of the 46 bugs within 1000 schedules
each, which is why the default policy, `mix`, alternates them. Of the rest:
`streamcluster2` has no checked failure at all (the missing barrier only
changes `output.txt`); `twostage_100` needs one of 99 identical threads to be
preempted inside a lock while the 100th, created last, runs first;
`reorder_N` for larger N needs the single checker thread, created after all
N setters, to run between two stores of one of them; and `safestack`'s ABA
corruption needs about five ordered, instruction-exact preemptions. All of
these are a matter of budget (SCTBench's own evaluation used 10,000
schedules), and the three work-stealing queues, `wronglock` and `reorder_3`
were only reachable at all once watchpoints made straight-line stores
scheduling points. A branch costs 5–50 ms for the small programs and about
a second for `pbzip2` and `streamcluster2`.

## Limitations

* x86-64 Linux only; needs a working PMU (`ffs check`). Inside most VMs there
  is no deterministic counter; `--ticks none` still gives syscall-level
  scheduling control and checkpoints, but no preemption.
* External input is live: file contents, the network, the terminal, and
  `timerfd`, which is not virtualized (a program blocked in `epoll_wait` on a
  timerfd waits real time). Datagram socketpairs keep only their first
  buffered datagram across a checkpoint; sockets connected by `connect()`
  rather than `socketpair()` stay shared with the original.
* `/proc/self/exe`, `/proc/self/cmdline` and similar reflect the copy; CPU
  times (`times`, `getrusage`) are real.
* Threads blocked in uninterruptible syscalls cannot be checkpointed; a thread
  about to exit or with a signal pending is run first by the explorer.
* A run ends when the main process has exited (as `rr record` does): daemons
  it left behind are killed. A deadlock is reported only while it is alive.
* Replaying a long busy loop is slow: the overflow interrupt lands up to
  10,000 ticks late on AMD Zen (rr assumes the same), and each tick of that
  window is replayed by breakpoint or single step.
* `exec` from a non-leader thread, 32-bit syscalls (`int 0x80`) and a
  program's own `ptrace` are not supported.
* Whether a syscall is reported as `Blocked` or as completed is decided by
  watching the thread for a short grace period, so the *exploration* itself
  (which schedules get tried for a given `--seed`) can differ between runs
  on a loaded machine. Every recorded schedule and every verification is
  exact regardless.
* Only four hardware watchpoints exist, and only globals of the executable
  are sampled; races on heap objects are reached through tick budgets and
  instruction steps, which need more branches.

## Layout

```
src/ptrace.rs      raw ptrace/waitpid wrappers, siginfo, /proc signal masks
src/mem.rs         /proc/pid/mem access, /proc/pid/maps
src/perf.rs        PMU detection, per-thread tick counters
src/task.rs        a traced thread and its stop state machine
src/session.rs     spawning, the serial run loop, events, signal replay
src/vtime.rs       virtual time: sleeps, timeouts, timers
src/random.rs      virtual randomness
src/proc.rs        emulated process tree (wait4/waitid/getppid/SIGCHLD)
src/ns.rs          private pid namespace, pid steering
src/remote.rs      syscall injection
src/fdpass.rs      SCM_RIGHTS descriptor passing into a tracee
src/checkpoint.rs  forking a whole session
src/sched.rs       policies (fifo, round-robin, random, replay) and the driver
src/explore.rs     random / DFS exploration from a checkpoint
src/trace.rs       trace format
src/vdso.rs        vDSO patching
include/ffs.h      tracee-side API
tests/c/           example programs with concurrency bugs and fidelity checks
tests/java/        JNI shim, Flink's FutureCompletingBlockingQueue, FLINK-37663 harness
tests/integration.rs
```
