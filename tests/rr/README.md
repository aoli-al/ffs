# rr's test suite under ffs

rr ships about 650 small C programs (`src/test/*.c` in the rr tree) that
exercise Linux syscalls, signals, threads and process semantics.  Each prints
a token, usually `EXIT-SUCCESS`, when its own checks passed, and rr's
harness requires that record and replay produce identical output.  This
directory runs the same programs under `ffs record` / `ffs replay` to
measure how far ffs is from rr's fidelity.

`tests.tsv` lists the tests: every entry of rr's `BASIC_TESTS` (x86 and
generic ones) plus the `TESTS_WITH_PROGRAM` entries whose `.run` script
reduces to "record, replay, check a token" (optionally with an
asynchronous signal sent during recording).  Tests that drive gdb, test
rr's command line, or need rr's syscall buffer library are not included.

```
nix-shell --run "python3 tests/rr/run.py --rr ~/repos/rr -j 12 --out /tmp/ffs-rr"
```

`run.py` generates rr's syscall-enum headers, builds every test against
rr's `util.h`, records, replays, replays again with `--strict` (syscall
return values compared) and writes `results.tsv` with one of these
statuses per test:

| status | meaning |
|---|---|
| `PASS` | token printed, replay followed the trace, output identical, strict replay identical |
| `STRICT_DIVERGED` | the second replay diverged (same checks; `--strict` is now an alias) |
| `OUTPUT_MISMATCH` | replay followed the schedule but printed something else |
| `REPLAY_DIVERGED` | the replayed events differ from the trace |
| `REPLAY_SHORT` / `REPLAY_ERROR` / `REPLAY_TIMEOUT` | replay ended early, ffs failed, or hung |
| `REC_NOTOKEN` | the program ran but failed its own checks under ffs (fidelity bug, or it needs an rr feature) |
| `REC_DEADLOCK` | ffs reported every thread blocked |
| `REC_ERROR` / `REC_TIMEOUT` | ffs failed or hung while recording |
| `BUILD_FAIL` | does not compile here |

Per-test work directories (`out/work/<name>`) keep the binary, the trace
and the stdout/stderr of each phase.  `--only a,b` restricts the run,
`--ffs-args "..."` passes options such as `--real-time` to ffs.
`--verify-outputs` adds output-buffer checking to both replay passes (not to
recording):

```
nix-shell --run "python3 tests/rr/run.py --verify-outputs -j 8 --out /tmp/ffs-rr-verify"
```

ffs uses live shadow execution, so this suite can expose external-state
mismatches as well as framework bugs. In particular, files left by recording
and metadata for freshly created pipes can differ on replay. The historical
baseline predates mandatory return checks; `--strict` is now a compatibility
alias.

rr is MIT licensed; the sources stay in the rr checkout and are built from
there (`--rr`, default `~/repos/rr`).
