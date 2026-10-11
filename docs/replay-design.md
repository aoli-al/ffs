# Shadow execution with checkpoint branching

Status: implemented replay contract, 2026-10-11. Coverage limitations remain;
see README and `tests/rr/GAP.md` for tests and supported facilities.

## Contract

ffs is a concurrency testing framework. The user is responsible for repeatable
external inputs and for avoiding, isolating or resetting file/network side
effects between executions. A saved trace supplies scheduling decisions and
observations to check. It does not supply replacement external inputs.

On replay:

1. Follow the recorded thread choices and instruction-level preemption points.
2. Execute ordinary syscalls against live kernel state.
3. Apply the same time, randomness and process-identity virtualization used
   during recording. Facilities already emulated by ffs (for example virtual
   sleeps and process-tree waits) retain their normal emulation in both modes.
4. Compare each syscall's tracee-visible return value, including errors and
   child identity, with the recorded result. Report divergence at the first
   mismatch. Do not patch a mismatching live result to match the trace.
5. With `--verify-outputs`, also compare captured output addresses and bytes
   before another thread is resumed.
6. Reject incomplete replay or a different terminal outcome.

For example, if a recorded `read` returns 8 but the live read returns 1, replay
fails at that syscall. If it returns eight different bytes, the default scalar
check accepts the return value and `--verify-outputs` detects the buffer
mismatch. A file created during recording that causes `EEXIST` during replay
is an external-state mismatch to be fixed by the test's setup/cleanup.

## Framework responsibilities

Time, randomness, thread/process identity and scheduling are ffs's concern.
Time virtualization is driven by tracee progress. PID namespaces provide
stable kernel-visible pids/tids; the emulated process tree handles parentage,
waits and SIGCHLD across checkpoints. These mechanisms run in record, replay
and exploration, rather than masking differences only during replay.

Trace version 2 preserves tick source, PMU name, slice-cap duration and use of
a private PID namespace. Replay restores the settings, checks the PMU and
fails if a required namespace is unavailable. Environment and cwd also come
from the trace.

Asynchronous-memory interfaces that cannot be controlled at syscall stops
are refused with `ENOSYS`. Timerfds are refused under virtual time until their
counter and readiness semantics are virtualized. Compat and x32 syscalls are
rejected before interpreting them using native x86-64 output descriptions.

## Trace and checks

A step contains `(ltid, event, ticks, rip)`. Syscall exits contain the syscall
number, return value and optional logical child identity. Version 2 also
carries optional `effects.out` entries with output addresses and bytes.

The output table in `src/effects.rs` covers the syscall subset listed in the
README. Bytes are currently stored inline in JSON, limited to 64 MiB per
captured syscall. Missing effects mean **unverified**, not an empty output.
Partial output writes from failed syscalls and unspecified syscalls are not
covered. Broader descriptions and an output-table audit can improve this
coverage without changing the execution model.

Output checks are opt-in because some returned metadata reflects uncontrolled
kernel state even for an otherwise well-behaved test: an inherited stdout
pipe may have a different inode number. The checker reports those differences;
it does not normalize external metadata or substitute recorded bytes.

Return checks are enabled by default, including for legacy v1 traces.
`--strict` remains a compatibility alias. `--lenient` is an explicit legacy
v1-only escape hatch; it does not satisfy the checked-return replay contract.
Older experimental v2 `inject` and `file_offset` fields are ignored when
reading a trace. No syscall is skipped or offset repaired because of them.

## Branching

Checkpoint branches retain real descriptors, offsets, queues and process
state. Replay performs their state transitions through actual syscalls, so
there is no replay-only fd reconstruction or input-injection path. Once a
branch leaves the recorded schedule it resumes ordinary exploration and
records its own observations.

Existing file snapshots during exploration remain a limited convenience. They
do not make arbitrary external resources reproducible, and they do not
remove the user's obligation to supply a controlled test environment.

## Remaining work

* Extend and test time/identity coverage, including timerfd semantics.
* Make recording preemptions exact and blocking/readiness decisions independent
  of host timing. Removing external side effects alone does not make repeated
  same-seed runs identical: overflow skid, wall-clock slice caps and kernel
  readiness still matter.
* Expand optional buffer checks and improve mismatch diagnostics.

Known synchronous queries and regular-file I/O are exempt from host-timed
blocking classification. `--slice-cap-ms 0` disables host-time slice
preemption, but branchless loops may then run forever.

Self-contained rr-style replay, external-input injection, filesystem archives
and a replay mode without live kernel state are not goals of this contract.
