# bbtree under ffs

[bbtree](https://github.com/XiangpengHao/bbtree) (v0.1.2, commit `6619c4e`,
Oct 2026) is a B+-tree whose shared map, `bbtree::BBTreeMap`, is an
optimistic-lock-coupling tree: internal nodes are read without a latch and
checked by version afterwards, leaves are latched (shared or exclusive)
with a spinning backoff, and every failed check restarts the operation
from the root (`doc/09-concurrency.md`). The crate is `no_std`, so
contention never blocks in the kernel: every wait is a spin loop. Its own
concurrency testing is Shuttle (`src/map/shuttle_tests.rs`): 22 scenarios,
each a tree one operation away from a change of shape, two or three
threads raced over it, run under thousands of random and PCT schedules
with switch points at every latch access the tree marks
(`src/tree/interleaving.rs`).

## Setup

```
./run.py setup --bbtree ~/repos/bbtree-src/bbtree   # a checkout
./run.py list
./run.py explore --iterations 2000 --seeds 1 2
./run.py explore separator_lookups_race_a_merge --watch root --watch-hold 0.5
./run.py native --runs 2000
./run.py replay out/<scenario>/<tag>/fail-1.json
```

`setup` symlinks `ffs_tests.rs` into the checkout as `src/ffs_tests.rs`,
adds `#[cfg(all(test, ffs))] mod ffs_tests;` to `src/lib.rs`, widens the
two `cfg(shuttle)` gates on `node_lengths`/`leaf_keys` (the shape probes
the scenarios need) to `cfg(any(shuttle, ffs))`, and builds the crate's
unit-test binary with `--cfg ffs` in release with debug assertions and
overflow checks on (stable cargo 1.98 suffices; 45 s). Nothing else in the
crate changes.

## The harness

`ffs_tests.rs` is the Shuttle file ported to real threads: the same
`Tracked` values (wide, with drop flags in an arena that outlives the map,
so a closure that runs on a dropped or foreign value is caught), the same
`Shapes` probe, the same 19 small scenarios and 3 model-checked ones with
random operations against per-thread models. Differences:

* each scenario builds its tree, then calls `ffs_checkpoint("race")` and
  spawns its threads: `ffs explore` branches from there, so the setup
  (hundreds of inserts) is paid once;
* `Tracked::check` calls `ffs_point()` where Shuttle's version calls
  `yield_now()` (a scheduling point, not a yield: see below);
* the model threads' operations come from a seed (`FFS_BBTREE_SEED`,
  `FFS_BBTREE_OPS` for the count), the same in every branch;
* the map allocates through a `Pages` allocator that records the root page
  (the first allocation, kept for the map's life) and the last page
  allocated before the race; `FFS_BBTREE_WATCH=root|last` registers their
  latch words with `ffs_watch()`, so every access to the latch is a
  scheduling point and `--watch-hold` can keep a thread inside its
  latch read-modify-write while another reaches it. The latch's offset in
  the page is found by reading the first two words (the one that is a
  nonzero multiple of 2^21 is a version; `LatchedNodeHeader` is not
  `repr(C)`), 8 here. The allocator also keeps every page out in a
  lock-free table (no futex of the harness's own inside a split), with
  the thread that allocated it: a page given back twice, a page given back
  while threads share the map by a thread other than its allocator (pages
  must stay type-stable until the owner drops the map; a thread may free a
  page it allocated and never published, the root of a tree another thread
  started first), and a page never given back all panic.

Nine more scenarios race paths the Shuttle file leaves alone:

* `copied_lookups_race_the_root_lowering_and_raising` and
  `copied_lookups_race_a_split_and_updates`: a `u64 -> u64` map, so
  `get_copied`, `get_many_copied` and `contains_key` take the latch-free
  path (copy the value, check the leaf's version after), which `Tracked`
  values never do; every copy must be the key's value, its updated value,
  or nothing;
* `racing_inserts_of_one_key_return_each_old_value_once`: three threads
  insert the same key into a splitting leaf; the returned old values plus
  the kept one are exactly the inserted ones, each once;
* `get_or_insert_races_removals_of_the_same_key`: what `get_or_insert`
  sees was the map's value at that moment;
* `batched_inserts_race_scans_and_a_merge`: `insert_many` over eight
  leaves against scans and a merge;
* `breaking_scans_and_is_empty_race_the_tree_emptying_and_refilling`:
  scans that break at their first entry, and `is_empty`, while the tree
  empties and is refilled past a split;
* `panicking_closures_let_other_threads_through`: closures that panic
  inside `update`, `read` and `remove_if`, each caught, while another
  thread reads and inserts into the same leaf (a leaked latch would spin
  that thread to the depth bound, which `run.py` reports);
* `six_threads_match_their_models_around_one_leaf`: the model test with
  twice the threads;
* `tied_lookups_race_removing_and_restoring_a_head`: string keys whose
  4-byte windows tie in the internal node, looked up around a leaf head
  while it is removed and reinserted.

A scenario runs in 0.01 s natively and a branch under ffs takes about
15 ms (150-400 steps; the model scenarios with 60 operations per thread
about 1300 steps).

## Deeper: shared keys, linearizability, owner walks

The model scenarios give each thread keys of its own, so no thread ever
sees what another does to a key: a lost update, a stale copy, a value
returned to two threads would pass them. The second round adds what
catches those.

**Linearizability of shared keys** (`SharedKeysRun`): four to six threads
work on the same ten-odd keys, half of them present, around a leaf that
splits and merges (the middle and end of a full root leaf; the last leaves
of a tree of three levels, which lowers and is raised again), with
`insert`, `remove`, `update`, `remove_if`, `get_or_insert`, `read` or
`get_copied`, `contains_key`, `read_many` or `get_many_copied`,
`insert_many`, `scan`, `scan_rev` and `range_cloned`. Every call is
stamped with a global counter before and after; after the race each key's
history is checked against a register (insert and remove return the value
they replaced, `update` the one it changed, `get_or_insert` the present
one or its own, lookups the current one, `remove_if` removes exactly when
its condition holds of the current value) by a depth-first search over the
orders the stamps allow (Wing and Gong's algorithm, states memoized). An
entry a scan hands out is a lookup during the scan; an entry a cloned
range hands out, a lookup during that `next()`; a batch's lanes are
lookups or inserts during the batch; the final value of each key, a
lookup after everything. A map operation touches one entry, so the
history is linearizable if every key's is. Values are told apart by an
origin that survives `clone` (`Tracked::origin`; `u64` values carry the
key and a count), so a copy handed out is checked as the value it was
copied from. Five scenarios: `shared_keys_are_linearizable_around_a_splitting_leaf`,
`..._in_three_levels`, `copied_lookups_of_shared_keys_are_linearizable`
(`Copy` values, the latch-free copies), `shared_string_keys_are_linearizable_at_tied_separators`,
`six_threads_share_keys_of_a_splitting_leaf`; `FFS_BBTREE_OPS` and
`FFS_BBTREE_THREADS` (or `--ops`, `--threads`) scale them.

**Blind spots of the rounds above** (asked for after the campaign), four
scenarios and three additions to the shared-key mix:

* `lowering_through_four_levels_races_appends_and_lookups`: a tree of
  four levels (27,000 entries, `DeepShapes`) lowers to three while appends
  raise it, under lookups, batched lookups and scans of its last keys:
  every change passes two internal levels;
* `shared_byte_array_keys_shrink_the_prefix_while_linearizable`:
  `[u8; 12]` keys under the `WithInlineKey` strategy, sharing eight
  leading bytes, with three outliers sharing seven, five and two: each
  outlier's first insert shrinks the shared prefix under the other
  threads' calls, all linearizable;
* `panicking_clones_scans_and_batches_let_other_threads_through`: a
  `Clone` that panics inside `get_cloned`, `range_cloned` (both ends) and
  `get_many_cloned`, and closures that panic inside `scan`, `scan_rev`,
  `read_many` and `get_or_insert`, while another thread reads, clones and
  inserts into the same leaf;
* `debug_formatting_races_a_split_and_a_merge`: `format!("{map:?}")`
  while the root leaf splits and the tree lowers back: in order, no steady
  key lost;
* the shared-key threads now also call `get_cloned`, `get_many_cloned` and
  `remove_entry`, and scan ranges with bounds of every kind (included,
  excluded, open, empty) between two shared keys, checking that nothing
  outside the bounds is visited.

Not testable this way: allocation failure (`handle_alloc_error` aborts the
process by design), and weak memory ordering (ffs runs one thread at a
time; a missing fence in the optimistic reads needs Miri or Loom).

**Owner walks after the race.** Every model scenario now also walks what
the race left with the owner's code (`iter_mut` from both ends, then
`into_iter`, which takes the tree apart), and expects exactly the entries
the models hold, every value live: the unsync walks run on a tree that
concurrent splits, merges, raises and lowerings built.

**Pages.** The allocator's accounting above holds in every scenario,
old and new: no page leaks, is freed twice or goes back to the allocator
while shared.

## Results

**One bug found** (Oct 19 2026, by the blind-spot round below): a scan or
cloned range whose start key lacks the tree's shared prefix can visit
entries outside its bounds while another thread's insert shrinks the
prefix. Everything else passes under every schedule tried: about 600,000
branches over two rounds, four policies, hardware watchpoints, exact
preemption, up to eight threads and 300 operations per thread, with the
linearizability, page-accounting and owner-walk checks of the second round.

### The bug: scans from outside the shared prefix race a shrink

`scans_from_below_the_shared_prefix_race_a_shrink` (and its mirror,
`reverse_scans_from_above_the_shared_prefix_race_a_shrink`): a map of
`[u8; 12]` keys that share `shared/0`; one thread scans from
`share_/00000` (inclusive), which lacks the shared prefix and sorts below
every key, with `scan` and with `range_cloned`; another inserts
`sh____/00000`, which lacks the prefix too, shrinks it to `sh`, and sorts
below the start. The scan visits `sh____/00000`, and the cloned range
hands it out: an entry below the range's inclusive start.

| | failing |
|---|---|
| ffs, root-latch watchpoint, `--watch-hold 0.5`, 300 branches | 75 (scan 49, cloned range 26) |
| ffs, mirror scenario, same | 80 |
| native, 300 runs | 1 (mirror: 3) |
| sequential form (`scans_from_below_the_shared_prefix_sequentially`) | 0: not a bug of the sequential logic |

Found first in `shared_byte_array_keys_shrink_the_prefix_while_linearizable`
(2 of 1000 branches under both watchpoints; `out/KW-.../fail-1.json`
replays it), where a scan from `share_/00050` visited `sh____/00050`.

Why (`tree/latched/scan.rs`, `scan_start`; `descent.rs`, `read_root`):
the descent for the start key reads the shared prefix from the root page,
finds the key lacks it, places the key outside all keys (`place_outside`)
and reports `Outside` once the root's version vouches for the prefix it
read. `scan_start` then takes the "before every key" answer and calls
`descend_to_first_leaf`, a second, independent descent that validates its
own path but not that the verdict still holds, and starts a `whole_leaf`
cursor at that leaf's edge. Between the two descents the shrink (which
holds the root and both borders, bumping their versions) and the insert of
the outlier complete; the first leaf now begins with a key below the start,
and the scan visits it. The lookups are safe (an `Outside` verdict ends
them with "absent", which was true at the moment it was vouched for); a
scan turns the verdict into a position and keeps using it after the state
it was vouched for is gone. The mirror holds for `scan_rev` and a key above
every key, at the last leaf.

A fix: after `descend_to_first_leaf`, compare the root's version with the
one that vouched for `Outside` and restart on a change (the root's version
changes with every shrink); or place the cursor in the first leaf by
comparing the start key with the leaf's keys (whole keys, as the leaf's
inline keys assume the prefix the key lacks) instead of `whole_leaf`.

### The scenarios of the first round (Oct 18 2026)

Every one of them passes under every schedule tried:

| campaign | branches | failing |
|---|---|---|
| `mix` policy, seeds 1 and 2, no watchpoints | 22 × 2 × 2000 | 0 |
| watchpoint on the root page's latch, `--watch-hold 0.5` | 22 × 2000 | 0 |
| watchpoint on the last page's latch, `--watch-hold 0.5` | 22 × 2000 | 0 |
| model scenarios with 60 operations per thread, seeds 3 and 4 | 3 × 2 × 500 | 0 |
| native, the whole suite repeated | 22 × 2000 | 0 |
| the nine added scenarios, `mix`, seed 1 | 9 × 2000 | 0 |
| the nine added scenarios, root-latch watchpoint, `--watch-hold 0.5` | 9 × 1000 | 0 |
| the nine added scenarios, native | 9 × 500 | 0 |

### Mutants: the scenarios do catch what they target

Two one-line mutations of `src/tree/node/latch.rs`, the classic
optimistic-lock-coupling mistakes, were built into separate binaries and
run with `./run.py explore --binary ... --tag m1-` (300 branches per
scenario, seed 1) and `./run.py native --binary ... --runs 3000`:

* **M1**: `is_unchanged` always true (an optimistic reader never notices
  a writer);
* **M2**: `unlock_exclusive` stores the version it took instead of the
  next one (a writer's change is invisible to version checks).

| | M1 ffs | M1 native | M2 ffs | M2 native |
|---|---|---|---|---|
| failing runs of 6600 (300 a scenario) | 668 | 211 | 635 | 171 |
| scenarios that fail at all (of 22) | 15 | 10 | 13 | 9 |

ffs fails three to four times as often per run, and finds the mutants in
scenarios native stress never does in 300 runs: `splits_race_to_raise_a_full_internal_root`
(31 and 21 branches of 300 against 0),
`removing_the_roots_separator_lowers_three_levels` (39 and 35 against 0
and 3), `internal_nodes_take_a_child_from_a_full_sibling` (8 and 7 against
0). Natively the mutants also deadlock now and then (a 3000-run baseline
hung for an hour), which `run.py native` now cuts off at 20 s.

The failures are the ones the scenarios were written for: `a closure ran
on key 70's value after the map dropped it` (a lookup through a freed
leaf's bytes), `visited out of order, or twice` and `missed 20` (scans
across a split or merge), `assertion failed: index < self.len()` in
`leaf.rs`, `inserting at edge 48 of an internal node of 1 separators`,
`entered unreachable code: a key in the tree shares its prefix`, a
SIGSEGV. The seven scenarios that never fail even on the mutants
(`appends_race_*`, `emptying_the_tree_races_inserts`,
`racing_get_or_inserts_see_one_entry`, `racing_removals_take_an_entry_once`,
`lookups_race_removing_the_roots_separator`,
`shrinking_the_shared_prefix_races_raising_the_root`) race writers against
writers, or lookups that end on a latched leaf, which neither mutation
touches.

## Audit

A reading of `src/tree/latched/` (descent, lookup, insert, append, remove,
scan, shared_prefix, pages, held, batch) and `map/range.rs` for windows
where a thread acts on state another is changing. What holds, and why:

* **Optimistic reads** follow the OLC rule exactly: a node's version is
  read before its contents, an edge is followed only after the node's
  version is re-checked, a child's version is read before its parent's
  check, and a leaf (latched or read at a version) is checked against its
  parent after. Versions only grow, so a page freed and reused cannot pass
  a stale check (ABA needs 2^43 changes).
* **The root page** changes kind in place; readers re-read the root
  pointer's tag after reading the root's version (`root_version`), and a
  root leaf is checked against the pointer once latched (`is_root`).
* **Type-stable pages**: a freed page goes on a free list for its kind and
  its link is written into the inline-key slots, which a stale reader
  treats as data; the length it reads is clamped. A split that pops a
  free page latches it *waiting*, which is deadlock-free only because a
  thread that reaches a free page through a stale pointer never waits
  while holding it: the descent, `read`'s `latch_unchanged` and the scan's
  `walk_down` all check a version right after the latch and let go.
* **The last-leaf pointer** is changed only by a thread holding that leaf
  exclusively (`split` with `was_last_leaf`, a removal that freed it);
  `append` latches the leaf the pointer names, then re-reads the pointer.
  A freed last leaf's page may be latched by an append after it is on the
  free list: harmless, the pointer check fails and the latch is let go.
* **The shared prefix** only shrinks (`SharedPrefix::shrink_to`) or starts
  anew in an empty root leaf, both with the root and both borders held, so
  a key checked against it at the root still shares it when inserted into
  an interior leaf later; a border leaf is held by the shrink, which a
  concurrent insert then finds changed.
* **Scans** latch the next leaf before letting go of the current one, and
  only readers wait for latches while holding one, so writers (which take
  everything but their own leaf and fresh pages without waiting) and scans
  form no cycle.
* **Cloned ranges** validate buffered copies with leaf stamps (version
  numbers) before handing them out, and re-scan past the last key handed
  out otherwise.
* **Batched inserts** re-latch each destination leaf by compare-and-swap
  at the version the lane read, and advance the version for later keys of
  the same batch exactly by the insert's own bump.

Nothing read contradicts the design's claims. The arguments that rest on
the most moving parts became scenarios, each run with the hardware
watchpoint on the latch word of the page about to be freed
(`--watch last --watch-hold 0.5`) besides the root's and none:

* `appends_race_the_last_leaf_freed_and_its_page_reused`: appends wait
  on the last leaf while a merge frees it and a split takes the page back;
* `latched_reads_race_a_leaf_freed_and_reused_for_other_keys`: `read`
  latches a leaf at the descent's version while a merge frees it and a
  split refills the page with other keys, under a scan;
* `internal_pages_freed_and_reused_under_lookups_and_scans`: a merge of
  internal nodes frees an internal page, appends raise the tree back and
  reuse it, while lookups descend through the changing levels;
* `copied_lookups_race_pages_churning`: latch-free copies while the tree
  lowers into its root leaf and splits again, twice, the freed page reused
  each time.

| | branches | failing |
|---|---|---|
| the four audit scenarios, `mix`, seed 1 | 4 × 2000 | 0 |
| same, watchpoint on the page about to be freed, `--watch-hold 0.5` | 4 × 1000 | 0 |
| same, watchpoint on the root's latch, `--watch-hold 0.5` | 4 × 1000 | 0 |
| same, native | 4 × 500 | 0 |

### The deep campaign (Oct 19 2026)

All 40 scenarios unless said otherwise; `mix` policy unless said otherwise;
`out/campaign/*.log` has each run. No failure of bbtree in any of them.

| phase | setting | branches | failing |
|---|---|---|---|
| A | seeds 21 and 22, 3000 each | 240,000 | 0 |
| B | watchpoints on the root's and the last page's latch, `--watch-hold 0.5` | 60,000 | 0 |
| C | `pct`, `--pct-depth 5` | 60,000 | 0 |
| D | `--max-insn-steps 256` | 40,000 | 0 |
| E | the five shared-key scenarios, 6 threads, 48 operations each, 2 seeds | 10,000 | 0 |
| E | same, 100 operations per thread (4 threads) | 2,000 | 0 |
| F | the four model scenarios, 150 operations per thread, 2 seeds | 4,000 | 0 |
| G | `--exact-preempt`, the 11 scenarios of this round and the audit's | 3,300 | 0 |
| H | shared keys, 6 threads, 48 operations, both watchpoints, hold 0.5 | 5,000 | 0 |
| R | `random` (pure random walks; `mix` above alternates them with PCT), all 44 scenarios | 88,000 | 0 |
| R | `random`, shared keys, 6 threads, 48 operations | 5,000 | 0 |
| R | `random`, both watchpoints, hold 0.5, all 44 scenarios | 44,000 | 0 |
| E | shared keys, 100 operations, 6 threads (rerun) | 500 | 0 |
| K | the four blind-spot scenarios, seeds 61 and 62, 3000 each | 24,000 | 0 |
| K | same, both watchpoints, hold 0.5 | 4,000 | 2 (the bug, in the byte-array scenario) |
| K | same, `random` | 4,000 | 0 |
| T8 | the six shared-key scenarios, 8 threads, 32 operations | 6,000 | 0 |
| L | the six shared-key scenarios with the cloning lookups, `remove_entry` and random bounds in the mix | 6,000 | 0 |
| R | `random`, the six shared-key scenarios | 12,000 | 0 |
| R | `random`, shared keys, 6 threads, 48 operations | 6,000 | 0 |
| E | shared keys, 300 operations per thread | 1,200 | 0 |
| native | the whole suite, 3000 runs | 47 × 3000 | 0, but the two bug reproducers: 1 and 19 of 3000 |

Four one-line mutants, 300 branches a scenario under ffs (`mix`, seed
41) and 2000 native runs of the whole suite, after this round's scenarios
were added (40 scenarios): M1 and M2 as above, and two aimed at the new
checks:

* **M3**: a scan's hop to the next leaf skips the re-check of the versions
  of the nodes below the hop point (`scan.rs`, `next_leaf`);
* **M4**: a cloned range hands its copies out without checking that their
  leaves' stamps are current (`range.rs`, `next_copied`).

| | ffs: scenarios failing (of 40) | ffs: branches (of 12,000) | native: scenarios (of 40) | native: runs (of 80,000) |
|---|--:|--:|--:|--:|
| M1 | 28 | 1,165 | 26 | 4,160 (+1 hang) |
| M2 | 30 | 1,178 | 18 | 4,806 |
| M3 | 0 | 0 | 0 | 0 |
| M4 | 5 | 474 | 5 | 842 |

M4 is caught by the five linearizability scenarios and nothing else, as
intended: a cursor hands out a value that another thread's removal or
update had replaced before the `next()` began, which only a cross-thread
history shows (`key 135: ... T3 Remove -> Some(33) ... T2 Read ->
Some(33)`). The shared-key scenarios are also the most sensitive to M1 and
M2 (143 to 192 of 300 branches each). M3 survives everything: the hop
walks down from the ancestor with the next edge and re-checks every node
it passes, and the nodes it skips lie on the held leaf's border, which
cannot move the neighbor while the leaf is held, so the check it removes
looks redundant (an equivalent mutant, not a gap in the scenarios).

The six-thread scenario at 100 operations first failed 297 of 500 branches
on the checker's own limit (128 calls on one key, a `u128` bitmask): the
bitset is unbounded now and that run was repeated (E, rerun, below).

## What it took on the ffs side

**A yield inside a critical section livelocks PCT.** The shared-key
scenarios took 25 s in about one branch of fifty (the others 40 ms): a
`remove_if` whose rebalancing removal must take a sibling leaf a scan holds
shared, and restarts; the scan and two writers spin for the leaf the
remover holds exclusively. `Tracked::check` ran inside the remover's
closure, under its latch, and called `ffs_yield()`, which PCT treats as
`sched_yield`: the thread waits, so it goes to the bottom of the
priorities, below the three spinners demoted before it. Each spinner then
burns a slice and is demoted again, the remover runs to its next check
(release, restart, re-latch, closure: with no scheduling point between
release and re-latch, no spinner ever sees the leaf free) and yields again:
21,000 rounds in one branch, 85,000 scheduling steps. Natively the window is
hit within a few polls; Shuttle sees every latch access. ffs now has
`ffs_point()` (`FFS_SYS_BASE + 9`, `Event::Point`): a scheduling point
with no claim that the thread waits, where the random policy may switch as
at any event and PCT keeps its priorities. `check` uses it; the scenario
runs 300 branches in 11 s instead of 50-200. The diagnosis came from
`strace -p` of the tracer (single-stepping all four workers in turn, no
thread asleep), `FFS_PCT_DEBUG=1` (the demotion cycle), and symbolizing
the tracees' stacks through `/proc/<pid>/mem` from outside (`gdb` cannot
attach to a traced thread).


**Exploration branches had no spin detection.** `ffs record` shrinks the
slices of a thread that is preempted repeatedly on the same page and
tells PCT to treat it as yielding (`SpinTracker` in `sched::run`), but
`explore`'s `run_branch` passed `spin_level: 0`. bbtree waits by spinning
(`Backoff` in `latch.rs`, `pause` loops) and restarts optimistic descents
from the root, so in every PCT branch where the spinner outranked the
latch holder it ran alone to the 100k-step depth bound (5 s a branch;
half the branches). The fix tracks each thread's run of consecutive
preemptions, on any page (a restarting descent runs a loop over many
pages, which the same-page test never saw), and PCT demotes the thread
once the run exceeds a **spin patience**, drawn log-uniformly from 1 to
256 per branch. A fixed small patience zeroed out the `order_violation`
integration test, which needs a thread to finish a 1M-iteration compute
loop before another starts: under strict priorities a long loop and a
spin loop look the same, so some branches must keep the looping thread.
Shrinking the spinner's slices, as `record` does, is also wrong for
exploration: the loop then takes 5x more steps and PCT's change points,
placed per step, land inside it in nearly every branch (detection went
from 1.7% to 0 of branches; without the cap and with the patience it is
0.4%). `FFS_PCT_DEBUG=1` prints every PCT demotion with its reason.

**libtest as the tracee.** The harness is bbtree's own unit-test binary
(`--exact ffs_tests::<name> --test-threads=1 --nocapture`): libtest's
thread, the test's thread and the scoped workers all run under ffs; a
panic in a worker fails the test and the process exits 101, which
`explore` reports as `exited(101)`. Nothing in ffs needed to change for
it.

**Heap watchpoints through an allocator.** The latch words live in pages
with no symbol; a custom `Allocator` (bbtree's own trait) is enough to
learn their addresses, with no change to the library.

## Files

```
ffs_tests.rs   the scenarios; symlinked into the checkout as src/ffs_tests.rs
run.py         setup, list, explore, native, replay, all
out/           results (gitignored): <scenario>/<policy>-w<watch>-h<hold>-s<seed>.log
               and .../fail-N.json (replayable); test-binary
```
