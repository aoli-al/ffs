# liquid-cache under ffs

[liquid-cache](https://github.com/datafusion-contrib/liquid-cache) (commit
`ead34d2`, Oct 2026) is a cache for DataFusion written in Rust: a lock-free
ART index (`congee`), an atomic memory/disk budget, FIFO eviction queues
behind a mutex, entries that move between Arrow-in-memory,
Liquid-in-memory and on-disk forms, and a key-value store (`t4`) with its
own I/O threads. Its concurrent core is `src/core/src/cache/core.rs`. The
project tests the budget and the eviction queues in isolation with
`shuttle`; the test that drives the whole cache concurrently
(`concurrent_cache_operations`) has its shuttle variant commented out.

## Setup

```
./run.py setup --lc ~/repos/liquid-cache-src/liquid-cache   # a checkout
./run.py leak                      # sequential reproducer, no ffs
./run.py mixed --iterations 150 --seeds 1 2
./run.py two-readers --iterations 300 --seeds 1 2
./run.py flush-evict --iterations 800 --policy random
./run.py replay out/mixed/fail-1/fail-1.json
```

`setup` builds `harness/` (its `Cargo.toml` is generated from
`Cargo.toml.in` with the checkout's path; the checkout's `Cargo.lock` is
copied so the dependency versions are the project's) inside the checkout's
`nix develop` shell, and patches one line of the checkout's `Cargo.toml`:
`t4` without its default `io-uring` feature. The io_uring backend cannot
run under ffs (`io_uring_setup` is `ENOSYS` there, as under rr); the
thread-pool backend is what liquid-cache's own macOS CI uses, and nothing
reported below is about the I/O backend.

## The harness

`harness/src/main.rs` builds a `LiquidCache` with a small memory budget
(3.5 entries by default, so eviction is constant), `AlwaysHydrate`,
`TranscodeEvict`, the default `LiquidPolicy`, pre-fills a few entries (and
flushes them to disk), then calls `ffs_checkpoint("ready")`. Each worker
thread (its own current-thread tokio runtime, like the project's own test)
runs a script of operations: `insert`, `get`, `prefetch`+`read_entry`,
`try_read_liquid`, `contains`, `stats`, `flush_all_to_disk`. Scripts are
either random (from `--seed`, the same in every branch) or explicit
(`--script "get:0:0,insert:1:0,flush"` per thread). Checks:

* a `get` returns exactly the array inserted for that (key, identity), or
  nothing (`ffs_assert` on the contents);
* no panic (every worker panic is joined and fails the run);
* after all workers have joined: `stats().memory_usage_bytes` equals the
  bytes of the entries the index holds, `total_entries` equals what
  `for_each_entry` visits, and (with `--check-disk`) the disk budget equals
  the on-disk entries' bytes.

`--watch-budget` registers the heap address of `budget.used_memory_bytes`
with `ffs_watch()` so every branch has a hardware watchpoint on it;
`--store-dir` names the t4 directory (tempdir names repeat under ffs's
virtual randomness, which breaks replay). A panic prints the cache's own
event trace. The default run takes 40 ms natively and 1.2 s under
`ffs record` (6 threads, 23k steps); a mixed branch is 0.1 s unless it
spins (below).

## Findings

### 1. Disk budget leak on hydrate-then-flush (sequential)

```
./run.py leak
[harness] prefilled 1 entries (0 mem / 1488 disk bytes)
[harness] final: ... disk_budget=5952 disk_entries=1488
ASSERTION FAILED: disk budget drift: budget says 5952 bytes used, entries hold 1488
```

insert → flush (DiskArrow, 1488 bytes charged) → `get` (AlwaysHydrate
puts a MemoryArrow back in the index; the blob and its 1488 bytes stay
charged) → flush again (`write_batch_to_disk` reserves another 1488 and
`store.put` overwrites the blob). Every hydrate/flush cycle leaks one
blob's worth; `remove_disk_entry` releases one. `try_insert` (core.rs)
handles the memory side of a replacement but never releases
disk bytes when a disk entry is replaced by a memory one. With a bounded
disk budget the phantom usage grows until `find_disk_victim` evicts real
entries and, once the Disk queue is empty, every write returns
`CacheFull`. This is deterministic; the harness's oracle found it on the
first native run, so the concurrency runs below use the memory checks only.

### 2. `try_insert` time-of-check/time-of-use: memory budget drift

```rust
// core.rs
let cached_batch_type = if let Some(slot) = self.index.get(&entry_id) {
    let old_memory_size = slot.entry.memory_usage_bytes();
    if self.budget.try_update_memory_usage(old_memory_size, new_memory_size).is_err() { ... }
    self.index.insert(&entry_id, identity, to_insert);
```

The old size is read from the index, the atomic budget is adjusted, then
the index is written; the three are not atomic. Two threads replacing the
same key in that window both account against the same old size:

* two readers hydrating the same on-disk entry both see `old = 0` and both
  reserve: `budget says 2368 bytes used, entries hold 1184`
  (`run.py two-readers`: two threads, one `get` each; 457 of 600 schedules
  with `ffs_watch` on the counter and `--watch-hold 0.7`, 0 of 1600 with
  neither, and 5 of 1100 with the hold but the watchpoint on a neighbouring
  word of the cache struct);
* `flush_all_to_disk` and an eviction of the same entry both see the
  MemoryArrow size and both subtract it (`run.py flush-evict`: `budget
  says 40 bytes used, entries hold 1120`; with the watchpoint and hold 659
  of 800 schedules end in one of the three crashes below).

In the mixed workload (3 threads, 20 random operations each, 6 keys, 2
identities) the drift shows up in 4 of 300 schedules, and in 2 of them the
counter goes below zero:

### 3. The budget counter wraps and the cache stops accepting inserts

```
thread 'worker-0' panicked at src/core/src/cache/budget.rs:40:12:
attempt to add with overflow        // used + request_bytes > max_memory_bytes
```

The harness is built with `overflow-checks`, so the wrap is a panic in
`try_reserve_memory`. In a normal release build `used_memory_bytes` wraps
to ~2^64 silently and every later `try_reserve_memory` fails: the cache
stays "full" for the rest of the process (inserts go to disk or return
`CacheFull`). Same root cause as 2, a double subtraction when a
`flush`/eviction/`drop_memory_entry` replacement races another replacement
of the same key. Found by `run.py mixed` (seed 1: 1 of 150 schedules) and
by `run.py flush-evict` (254 of 800). The other way round, an over-charged
budget, ends in

```
thread 'worker-1' panicked at src/core/src/cache/core.rs:363:17:
internal error: entered unreachable code: Unexpected batch in write_in_memory_batch_to_disk
```

(405 of 800 in `flush-evict`): `insert_inner` fails to insert, the policy
has no victims left, writes the entry to disk and retries with the
`DiskLiquid` entry, whose `try_reserve_memory(0)` still fails because
`used > max`; the loop then hands the disk entry to
`write_in_memory_batch_to_disk`, which cannot take one. So the insert path
has an `unreachable!()` that a budget already over its limit reaches.

### 4. A `get` reads a blob being rewritten under the same key and panics

```
thread 'worker-0' panicked at src/core/src/cache/core.rs:806:69:
create reader failed: IoError("failed to fill whole buffer", ...)
thread 'worker-0' panicked at src/core/src/liquid_array/array.rs:340:5:
assertion `left == right` failed: Invalid Liquid array magic number
  left: 4294967295  right: 1280394305
```

`read_disk_bytes` looks the key up in the index (say `DiskArrow`), then
reads the object from the store. Meanwhile another thread hydrates the same
entry, it is evicted again and written back under the same key, possibly
as Liquid bytes. The identity trailer liquid-cache appends to each object
guards against a *different identity's* bytes, not against the same
identity's bytes in the other format, and `t4::Store::put` releases the
old value's slot to its free list immediately (`release_value_space`), so a
later put of any key can reuse the slot while the reader's `pread` is still
queued on the I/O thread: the reader gets another value's bytes or a torn
mix. Either way `get` panics instead of returning the data (or a miss).
Found by the mixed workload (6 of 150 schedules in one earlier run, both
signatures); each failure was re-executed from the checkpoint and
reproduced.

### 5. `ArtIndex::for_each` panics when an entry is removed under it

```
thread 'worker-0' panicked at src/core/src/cache/index.rs:108:22:
Failed to get value from ART
```

`for_each` collects `keys()` and then `get`s each one with `expect`;
`remove_disk_entry` (disk full) or `drop_memory_entry` in between is a
panic. `stats()`, `flush_all_to_disk()` and `for_each_entry` are the
callers, so a stats poll during disk eviction crashes the process. Found by
the mixed workload (2 of 150 schedules in the same run as 4: the disk leak
of finding 1 fills the 1 MiB disk budget within a run, after which every
flush removes disk victims). `reset()` has the same shape with `unwrap()`.

Not bugs but worth knowing: `evict_victim_inner` pops the victim from the
policy's queue before trying to write it, and when `write_batch_to_disk`
returns `CacheFull` the entry stays in memory but is no longer in any
queue, so it can never be chosen again.

## What it took on the ffs side

* **io_uring**: `t4` mounts an io_uring by default; ffs answers `ENOSYS`,
  so `run.py setup` builds with the thread backend. Supporting io_uring in
  a deterministic scheduler is a project of its own (rr does not either).
* **Watchpoints on the heap**: the races above live in windows of a few
  hundred instructions with no syscall, in two threads at once. Tick
  budgets and single-stepping found them in the 60-operation mixed
  workload but not in the two-operation minimal scenarios (0 of 1600).
  A hardware watchpoint on `used_memory_bytes` stops a thread inside the
  window; the new `--watch-hold P` then keeps it off the runnable set so
  the other thread can reach its own access (RaceFuzzer's delay). The
  counter is a heap word with no symbol, so the harness registers its
  address with the new `ffs_watch(addr, len)` magic call (the address is
  the same in every run under ffs, but it differs by 224 bytes between
  `ffs record` and `ffs explore`, so it cannot be learned from a recording
  and passed as `--watch 0x...`).
* **Files**: the harness's tempdir was deleted at exit, which left ffs
  unable to restore the store file for the next branch (it now recreates
  the directory); and tempdir *names* repeat under virtual randomness, so
  a second run's `mkdir` fails with `EEXIST` where the first succeeded and
  the replay diverges. The harness takes `--store-dir` and `run.py` gives
  every run a fresh one.
* **Spinning branches**: about 1 in 4 mixed branches hits the depth bound
  (100k steps, 20 s) with t4's `IoWorker` thread spinning in
  crossbeam-channel's `array::Channel::start_send` (the "stamp not yet
  updated" `backoff.spin()` loop, which never yields) while the thread it
  waits for holds a slot mid-operation. `run.py mixed` uses
  `--max-depth 20000` to cap the cost; why the other thread does not get
  through is not understood yet (it is not the instruction stepping or the
  sampled watchpoints: both were switched off without effect).

## Files

```
harness/Cargo.toml.in   template; run.py writes Cargo.toml with the checkout path
harness/src/main.rs     the harness
run.py                  setup, scenarios, replay
out/                    results (gitignored): <scenario>/explore-<seed>.log,
                        fail-<seed>/fail-N.json (replayable), x<seed>/store
```
