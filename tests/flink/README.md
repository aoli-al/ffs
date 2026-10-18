# Flink's own tests under ffs

This directory runs Apache Flink's published test classes (the
`*-tests.jar` artifacts on Maven Central, Flink 1.20.3) under `ffs` with no
change to Flink and no call into ffs: the JVM is started through JUnit's
`ConsoleLauncher` with `--select-class`, and a failure is whatever the test
reports plus what ffs finds (deadlocks, hangs, crashes).

```
nix develop                                   # maven, jdk17 are in the shell
cd tests/flink
mvn -q -B dependency:build-classpath -Dmdep.outputFile=classpath.txt
./run.py --mode native                        # baseline, ~5 s per class
./run.py --mode record                        # one random schedule per class
./run.py --mode explore --iterations 20 --root vtime:0.05
./run.py --class SplitFetcherTest --mode explore --policy pct
./run.py --method 'org...FutureCompletingBlockingQueueTest#testConcurrency' --mode record
```

`run.py` starts each JVM with the flags Flink's own surefire configuration
uses on JDK 17 (`-ea` and the `--add-opens`/`--add-exports` list; without
them Kryo's reflection fails and `KryoSerializerConcurrencyTest` hangs,
natively too) in a fresh working directory holding only a private
`java.io.tmpdir`, recreated identically before a replay. `--replay` replays
each recording under the same conditions and reports `DIVERGED` if it does
not follow; `--verify-data` additionally compares the data every syscall
read, which finds the live input behind a divergence.

`pom.xml` only resolves the classpath (Maven does not resolve a dependency's
test-scoped dependencies, so the modules' test dependencies are listed
explicitly). `run.py` enumerates `*Test`/`*ITCase` classes in a module's
tests jar, runs one JVM per class (or method) with `-XX:-UsePerfData
-XX:+UseSerialGC -XX:TieredStopAtLevel=1`, classifies the outcome
(`PASS`, `TEST_FAIL`, `DEADLOCK`, `HANG`, `ASSERT`, `CRASH`, `DIVERGED`,
`TIMEOUT`, ...) and writes `results/<mode>/results.tsv` plus a per-class
`log.txt` and trace.

## What running Flink taught ffs

`FutureCompletingBlockingQueueTest` (15 tests, 0.4 s natively) was the first
real class. It records in about 16 s under the random and PCT policies and
replays exactly; getting there fixed four things:

* **Tracer throughput.** Per scheduling step ffs made one `waitpid` per
  blocked thread, read `/proc/<tid>/stat` for each, and issued five
  perf-counter ioctls per resume. Now one `waitpid(-1)` sweep collects all
  statuses, `/proc` is read only on the all-blocked path, and the counter
  stays enabled with only its period reprogrammed. Kernel time per step
  fell 3x.
* **Instruction stepping** after a tick budget costs 40% of a JVM
  recording and is off here (`--max-insn-steps 0`); it exists for
  straight-line races in small programs.
* **Busy-waiting under a serializing scheduler.** `testConcurrency` has
  five consumer threads polling `queue.poll()` in a loop. Natively the
  producers fill the queue on other cores within microseconds; serialized,
  each consumer spins out its whole slice every time it runs. That cost
  minutes of wall time, burned hundreds of virtual seconds (a 1M-tick slice
  is a millisecond; tests then time out), and under PCT, where the
  highest-priority thread always runs, the spinner starved everyone
  forever. Three changes: the default slice is 100k ticks; a slice is
  bounded whenever any other thread exists, not only when another is
  runnable, because a thread the spinner just unlocked is still on its way
  to its ptrace stop; and a spin detector shrinks the slices of a thread
  whose consecutive preemptions land in the same code page and makes PCT
  and FIFO treat it as yielding.
* **Cold replay of JVM traces.** Three more fixes before a recorded Flink
  class replayed: HotSpot parses `/proc/self/mountinfo` (mount ids differ
  per run, and so did its branch count), so a canonical copy is served;
  a thread the trace says blocks is now confirmed asleep in the kernel
  before the next thread runs (a recorded `futex` wake of one waiter woke
  nobody); and each step records its virtual time, because whether the
  recording jumped the clock while all threads were blocked depended on
  disk latency. Replays of this class (630k steps) take about 120 s
  against 16 s to record; replaying a preemption exactly costs breakpoints
  and single steps.
* **Temp files under virtual randomness.** Flink's RPC loader extracts a
  jar to `/tmp/flink-rpc-akka<UUID>.jar`; with virtual randomness every
  run draws the same UUID, so runs collided. `run.py` gives each run a
  private `java.io.tmpdir` and its own seed.
* **Deadlock detection with timer threads** and `--root vtime:SECS`, see
  the top-level README.

`JobDispatcherITCase` failed with `timerfd_create() failed: Function not
implemented`: Netty's native epoll transport, which Flink's REST server
uses, needs timerfds, and ffs refused them under virtual time. timerfds are
now virtual (deadline on the virtual clock, readability injected when it
arrives); that class and the seven `io.network.netty` test classes pass.
`AcrossCheckpointFileMergingSnapshotManagerTest` was reported deadlocked
while waiting on a 60 s timeout task: the detector now requires a timed
waiter to have fired three times before it counts as housekeeping.

Limits that matter at this scale: a trace is held in memory and written as
JSON at the end, and a run stops at `--max-steps` (10M) scheduling steps.
`BlobClientTest` (flink-runtime), which streams blobs over sockets, reaches
10M steps after 96 virtual seconds and about 4 minutes; its trace was 3.3 GB
before large read buffers were hashed. Streaming the trace to disk in a
compact format is the next step for I/O-heavy tests (status `STEP_BOUND`).

## Status (flink-connector-base, 24 classes, Flink 1.20.3)

| mode | result |
|---|---|
| native | 24 pass (`HybridSourceITCase` does not finish natively here and is skipped) |
| `ffs record` | 24 pass, 3 s to 60 s per class |
| `ffs record --replay` | 19 replay exactly; the 5 MiniCluster ITCases diverge late in the run by one or two ticks at a preemption, with no differing syscall input (see below) |
| `ffs explore --iterations 10 --root vtime:0.1` | 4 classes tried, including a MiniCluster ITCase: 10 branches each, no failure |

`SimpleStringSchemaTest` once diverged on `getdents64` returning 24 instead
of 128 bytes: HotSpot lists `/sys/kernel/mm/hugepages` at startup, and the
kernel stops filling the buffer after one entry when a signal is pending,
which ffs's preemption signal was, having fired at the `syscall`
instruction. Replay now queues the same signal before such a syscall
(recognizable in the trace as a preemption at the syscall's exit).

## Status (flink-core and flink-runtime)

Native baselines: flink-core 359 of 385 classes pass (13 serializer-upgrade
tests need resources from the source tree, 7 are abstract, 5 fail
natively); flink-runtime 949 of 1000 (28 fail natively, 13 abstract, 8 need
source-tree resources, 1 times out).

Under `ffs record --replay`, 171 flink-core classes checked so far all
behave as natively: 168 pass and replay exactly, 1 fails natively too, and
2 (`IntMinimumTest`, `OrderingTest`) replayed exactly once the
pending-signal replay was restricted to the syscalls it applies to. Under
`ffs record`, 551 flink-runtime classes checked so far: 544 behave as
natively (524 pass; the rest fail, are abstract or lack resources natively
too), and 4 I/O-heavy classes reach the 10M step bound (`BlobClientTest`,
`BoundedBlockingSubpartitionWriteReadTest`, `LocalInputChannelTest`,
`HashTableITCase`). No test behaves differently under ffs for any other
reason. `results.tsv` in an output directory has the per-class status.

The remaining ITCase divergences are a tick count off by one or two, deep
into the run, at a preemption, after hundreds of millions of ticks and
with every syscall input identical. The likely cause is the counter
itself: rr requires AMD's SpecLockMap optimization disabled (an MSR bit,
`zen_workaround.py`) for exactly reproducible retired-branch counts around
lock-prefixed instructions, which HotSpot executes constantly. A 200M-tick
lock-heavy C loop replays exactly here, so if the counter is at fault the
rate is low.
