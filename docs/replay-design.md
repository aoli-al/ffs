# Deterministic replay with branching: design (option 3)

Status: proposal, 2026-10-11.  Measured gap: `tests/rr/GAP.md`.

## Goal

Two properties that rr does not offer together:

1. **rr-grade replay.** A saved trace replays bit-exactly on the same CPU
   model, without the files, network or pids of the original run, and
   without any kernel state surviving from it.
2. **Branching anywhere.** From any step of a live or replayed execution,
   fork the session and run a different schedule, with real syscalls.

rr's replay has property 1 because it never executes most syscalls: it
injects recorded results into a process that has no fds, sockets or
children.  That is also why it cannot have property 2.  ffs today has
property 2 and a weaker form of 1: replay re-executes every syscall, so
everything the kernel hands out that is not a function of the schedule
(file contents, ids, directory order, external pipes) can differ.

The design keeps live re-execution as the execution model and adds two
logs to the trace: an **input log**, which makes re-execution independent
of external state while the kernel state stays live, and an **effect
log**, which allows an rr-style emulated replay when no live state is
wanted.  Both are recorded by the same mechanism.

## Three execution modes

| mode | syscalls | kernel state | can branch | deterministic given |
|---|---|---|---|---|
| **record / explore** (today) | all executed | live | yes | nothing; this is where traces come from |
| **re-execute** (today's replay + input log) | internal ones executed, external ones injected | live, equal to the original for internal objects | yes | trace + input log |
| **emulate** (new) | only address-space and thread syscalls executed, everything else injected from the effect log | none | no | trace + effect log, on the same CPU model |

A branch taken from a re-execution starts a new recording; its trace is
the prefix up to the branch point plus the branch.  A branch cannot be
taken from an emulation, exactly as in rr; to branch after the tool has
exited, frozen checkpoints have to be persisted (see "Persistence").

## Trace format v2

A step keeps `(ltid, event, ticks, rip)` and `SyscallExit` gains:

```
ret:   i64                      always recorded, always compared on replay
out:   [(addr, len, blob)]      bytes the syscall wrote into tracee memory
class: Exec | Verify | Inject   what replay does with this syscall
```

Blobs live in a side file (`trace.bin`, append-only, referenced by offset)
so the JSON stays readable.  The `out` list is the effect log; the subset
of steps with `class = Inject` is the input log.  A trace with no `out`
entries is a v1 trace and replays as today.

## Syscall classes

**Exec** always executes, in every mode: it changes the process in ways
that are not memory: `mmap`, `munmap`, `mprotect`, `mremap`, `brk`,
`madvise`, `clone`/`clone3`/`fork`/`vfork`, `exit`, `exit_group`, `execve`,
`arch_prctl`, `set_tid_address`, `set_robust_list`, `rseq`, `sigaltstack`,
`rt_sigaction`, `rt_sigprocmask`, `rt_sigreturn`, `prctl` (name, dumpable,
tsc), `membarrier`.  rr's "always execute" list is the reference.

**Verify** executes in re-execution and is checked against the record:
`ret` must match and the bytes it wrote must match the recorded `out`.
This is every syscall whose result is a function of session-internal state
and the schedule: `futex`, pipes and socketpairs created inside the
session, `eventfd`, `dup`, `close`, `fcntl`, `getpid`/`gettid`, `wait4`
(emulated already), `clock_gettime` (virtual already), `getrandom`
(virtual already).  A mismatch is a divergence, reported at the step.

**Inject** is skipped in re-execution and emulation; `ret` and `out` are
written into the tracee.  This is every syscall that reads the outside
world: `read`/`pread`/`readv` on external fds, `getdents64`, `stat` family,
`readlink`, `statfs`, `uname`, `sysinfo`, `getrusage`, `times`,
`ioctl` (per request code), `recvfrom`/`recvmsg` on external sockets,
`poll`/`select`/`epoll_wait` results involving external fds, `keyctl`,
`msgget`/`semget`/`shmget` ids, `adjtimex`.

Whether a `read` is Verify or Inject depends on the fd: ffs already tracks
which pipes and socketpairs are internal (`checkpoint.rs`,
`collect_pipe_ends`); regular files are external unless the session
created them after the trace started (then they are deterministic and
Verify).  The class is recorded per step, so replay does not have to
re-derive it.

**Refused**: `io_uring_setup`, `io_submit`, `userfaultfd`,
`process_vm_writev` into self, and `perf_event_open` return `ENOSYS`, as
rr does, because they write tracee memory asynchronously and cannot be
logged at a syscall boundary.

## Recording memory effects

rr knows the out-parameters of every syscall from `src/syscalls.py`, a
Python table of `arg = out(type)` annotations that generates its recorder.
It is data, not logic, and MIT licensed: generate a Rust table from it
(`build.rs` or a checked-in generated file) that gives, per syscall, the
argument registers that point to output buffers and how their length is
derived (`rsi` with length `rdx`, `rdx` with length `*r10`, a fixed
`sizeof(struct)`, an iovec array).  At the exit stop, ffs reads those
ranges with `/proc/pid/mem` and appends them to the blob file.  This
replaces nothing in the current session logic; it is a hook in
`handle_stop` for `SyscallExit`.

**Audit mode** (`--audit-effects`) checks the table instead of trusting
it: before resuming into a syscall, write `4` to `/proc/pid/clear_refs` to
clear soft-dirty bits; at the exit stop, walk `/proc/pid/pagemap` and diff
every soft-dirty page against a copy taken before the syscall.  Any
written byte outside the table's ranges is reported with the syscall
number.  It is slow (a full page-table walk per syscall) and is meant for
the test suite, where it finds table gaps automatically.  The same walk is
also the recorder of last resort for syscalls the table does not cover.

## Re-execution with an input log

Replay keeps `Replay` as the policy and adds three behaviours in
`handle_stop`/`pre_syscall`:

- An Inject step is skipped at its entry stop (`orig_rax = -1`, the same
  mechanism as the emulated sleeps in `vtime.rs`), and at the exit stop the
  recorded `ret` and `out` bytes are written.  The tracee never blocks on
  the outside world during replay.
- A Verify step executes.  If it blocks although the trace says it
  completed, which happens when internal state is the same but the kernel
  is slower, `interrupt_blocked` and the restart path handle it as today.
  If it completes with a different `ret` or different `out` bytes, that is
  a divergence: the program is not deterministic under the schedule, or
  the class table is wrong.
- `--strict` becomes the default; the current lenient comparison stays as
  `--lenient` for v1 traces.

Branching from a re-executed session is `fork_session` as today.  Kernel
objects are live: internal pipes hold the same bytes as in the original
run because every write to them was re-executed in the same order;
external fds point at whatever is there now, and the branch records their
reads into its own input log from that point.

## Emulated replay

`ffs replay --emulate` runs a v2 trace without live kernel state.  It
spawns the tracee as today (the `execve` is real, the initial address
space comes from the binary and ld.so, as in rr), then executes only Exec
syscalls and injects everything else from the effect log.  Thread creation
is real (`clone` is Exec) and the scheduler is `Replay`, so ticks and
`run_task_to` work unchanged.  Signals are already replayed from siginfo.

What is new relative to re-execution is the treatment of fds and files:
none are opened, so `mmap` of a file has to come from the trace.  rr
copies mapped files into the trace directory; v2 does the same for every
`mmap` with a file (`Exec` with a recorded blob of the mapped range, or a
copy of the file when it is large and read-only, the way rr's `pack`
does).  Shared mappings between processes are recreated as memfds, which
`privatize_shared_maps` already knows how to do.

Emulated replay is the mode for a trace recorded on another machine of
the same CPU model, or long after the files it read have changed.  It is
also the cheapest way to verify a failure: no file snapshots, no pid
namespace.  It cannot branch, and ffs says so if asked.

## Persistence (later)

A frozen checkpoint is a stopped process tree whose fds are regular
files, memfds, pipes and socketpairs.  CRIU can dump and restore such a
tree, including pipe contents and unix sockets, as long as it is not
traced at dump time: detach, dump, re-seize on restore, then
`fork_session` from the restored tree.  This gives "branch from this
point next week" and makes a saved checkpoint plus a branch trace a
complete, replayable artifact.  It is independent of the two logs and can
come last.

## What the rr suite should show after each stage

| stage | work | tests expected to flip (from `GAP.md`) |
|---|---|---|
| 0 | strict `ret` by default; refuse async-write syscalls | `aio`, `io_uring`, `userfaultfd`, `perf_event_mmap` |
| 1 | out-parameter table from rr's `syscalls.py`; record `out`; verify on replay | nothing flips yet; audit mode finds table errors |
| 2 | Inject class for external reads, stat, getdents, ids | `keyctl`, `msg`, `sem`, `shm_unmap`, `openat2`, `mmap_recycle`, `chmod`, `statfs`, `readdir`, `adjtimex`, `clock_adjtime`, `times` |
| 3 | signal delivery into an interrupted syscall on replay | `pause`, `ppoll*`, `intr_*`, `multiple_pending_signals*`, `two_signals_with_mask`, `sigprocmask_in_*` |
| 4 | `CLONE_VFORK`; tracee in its own process group; wait4 ordering | `vfork_*`, `clone_parent`, `clone_vfork*`, `64bit_child`, `exec_shared_as`, `abort`, `sigtrap_process_group`, `fork_many`, `shm`, `exec_stopsig` |
| 5 | stop handling without `/proc` polling | `hooks`, `rdtsc_loop*`, the signal-spam tests |
| 6 | emulated replay | the whole suite under `--emulate`, which is the real measure of property 1 |
| 7 | CRIU persistence | none; new integration tests |

Stages 3 to 5 were done on 2026-10-11 (see `tests/rr/GAP.md`: signal
delivery into interrupted syscalls, external SIGKILL replay, `vfork`,
job control, tracer signal handling, wait ordering, and a SIGCHLD-driven
wait loop at 60k syscalls/s).  Stages 0 to 2 remain the plan for the
"function of the trace" determinism statement.  Stage 2 is where the
determinism statement changes from "a function of the schedule and the
outside world" to "a function of the trace".

## Non-goals

Emulating `ptrace` for tracees, nested namespaces, and rr's syscall
buffer.  The first two are groups 1 and 2 in `GAP.md`; the third is a
performance technique that re-execution does not need because branches,
not replays, dominate an exploration.
