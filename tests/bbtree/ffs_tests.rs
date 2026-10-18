//! `BBTreeMap` under ffs: the scenarios of `map/shuttle_tests.rs` on real
//! threads, with ffs choosing the interleavings instead of Shuttle, and
//! nine more at the end of the file for what those leave alone (the
//! latch-free copies, batched inserts, same-key races, breaking scans,
//! panicking closures, six threads, tied string lookups).
//!
//! Each test builds a tree one operation away from a change of shape, marks
//! the exploration root (`ffs_checkpoint`), races two or three threads over
//! the change, and checks the shape and the invariants; the large ones run
//! random operations against models. Under Shuttle the switch points are
//! the latch accesses the tree marks (`tree/interleaving.rs`); under ffs
//! they are wherever the tracer preempts (conditional branches, single
//! steps, hardware watchpoints), so the tree runs exactly as released, with
//! its real spin loops and memory ordering, and a failing schedule replays.
//!
//! This file lives in ffs's repository (`tests/bbtree/`), symlinked into
//! the crate as `src/ffs_tests.rs` by `tests/bbtree/run.py setup`, which
//! also builds it:
//!
//! ```text
//! RUSTFLAGS="--cfg ffs --check-cfg cfg(ffs)" cargo test --release --no-run --lib
//! ffs explore -- target/ffs/release/deps/bbtree-* --exact ffs_tests::<name> --test-threads=1 --nocapture
//! ```
//!
//! Environment:
//!
//! - `FFS_BBTREE_WATCH=root|last|root,last`: hardware watchpoints on the
//!   latch word of the root page and of the page allocated last before the
//!   race (`ffs_watch`), so that every access to it is a scheduling point;
//! - `FFS_BBTREE_SEED`: the model threads' operations (the same in every
//!   branch; the schedule is what varies);
//! - `FFS_BBTREE_OPS`: how many operations each model thread runs.

use core::alloc::Layout;
use core::fmt;
use core::mem;
use core::ops::{Bound, ControlFlow, RangeBounds};
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::collections::{BTreeMap, BTreeSet};
use std::format;
use std::string::{String, ToString};
use std::thread;
use std::vec::Vec;

use crate::allocator::{AllocError, Allocator, Global};
use crate::inline_key::Auto;
use crate::map::BBTreeMap;

/// The tracee side of ffs: raw syscalls with numbers Linux does not know,
/// which the tracer intercepts (`include/ffs.h`). Natively each returns
/// `-ENOSYS` and does nothing.
mod ffs {
    use std::ffi::CString;

    const BASE: i64 = 0x0ff5000;

    unsafe fn raw(nr: i64, a: i64, b: i64) -> i64 {
        let ret: i64;
        // SAFETY: a syscall with no memory effects the kernel knows of; the
        // tracer reads `a`/`b` as the call's documented arguments.
        unsafe {
            core::arch::asm!("syscall", inlateout("rax") nr => ret, in("rdi") a, in("rsi") b,
                lateout("rcx") _, lateout("r11") _, options(nostack));
        }
        ret
    }

    /// Whether the process runs under ffs.
    pub fn is_active() -> bool {
        unsafe { raw(BASE + 5, 0, 0) == 1 }
    }

    /// The exploration root: every branch starts from this process state.
    pub fn checkpoint(name: &str) {
        let name = CString::new(name).unwrap();
        unsafe { raw(BASE + 1, name.as_ptr() as i64, 0) };
    }

    /// A scheduling point.
    pub fn yield_now() {
        unsafe { raw(BASE + 3, 0, 0) };
    }

    /// A line in the tracer's log, prefixed with the thread.
    pub fn log(msg: &str) {
        let msg = CString::new(msg).unwrap();
        unsafe { raw(BASE + 4, msg.as_ptr() as i64, 0) };
    }

    /// A hardware watchpoint on the 8-byte slots of `[addr, addr + len)` in
    /// every branch; to be called before the root.
    pub fn watch(addr: usize, len: usize) {
        unsafe { raw(BASE + 8, addr as i64, len as i64) };
    }
}

/// What the environment asks of a test.
struct Settings {
    watch_root: bool,
    watch_last: bool,
    seed: u64,
    operations: Option<usize>,
}

impl Settings {
    fn from_env() -> Self {
        let watch = std::env::var("FFS_BBTREE_WATCH").unwrap_or_default();
        let targets: Vec<&str> = watch.split(',').map(str::trim).collect();
        Self {
            watch_root: targets.contains(&"root"),
            watch_last: targets.contains(&"last"),
            seed: std::env::var("FFS_BBTREE_SEED")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1),
            operations: std::env::var("FFS_BBTREE_OPS")
                .ok()
                .and_then(|s| s.parse().ok()),
        }
    }
}

/// The global allocator, remembering the first page it handed out (the
/// root's, which the tree keeps for its whole life) and the last one: the
/// pages whose latch words a test may watch.
struct Pages {
    root: AtomicUsize,
    last: AtomicUsize,
    count: AtomicUsize,
}

impl Pages {
    fn new() -> Self {
        Self {
            root: AtomicUsize::new(0),
            last: AtomicUsize::new(0),
            count: AtomicUsize::new(0),
        }
    }

    /// The latch word of the page at `page`: the one of the first two
    /// words that holds a version (a multiple of 2^21, bumped by every
    /// exclusive unlock) rather than the node's `u16` header fields.
    /// `LatchedNodeHeader` is not `repr(C)`, so the field order is the
    /// compiler's.
    fn latch_word(page: usize) -> usize {
        const VERSION_UNIT: u64 = 1 << 21;
        let words: [u64; 2] = [0, 8].map(|offset| {
            // SAFETY: the page is allocated (at least a header long) and
            // no thread runs on it while the test reads it, before the race.
            unsafe { ((page + offset) as *const u64).read_volatile() }
        });
        let is_version = |w: u64| w != 0 && w % VERSION_UNIT == 0;
        match (is_version(words[0]), is_version(words[1])) {
            (true, false) => page,
            (false, true) => page + 8,
            _ => {
                ffs::log(&format!(
                    "[harness] cannot tell the latch word of page {page:#x} ({:#x}, {:#x}); assuming offset 8",
                    words[0], words[1]
                ));
                page + 8
            }
        }
    }

    /// Registers the watchpoints the settings ask for.
    fn watch(&self, settings: &Settings) {
        let root = self.root.load(Ordering::Relaxed);
        let last = self.last.load(Ordering::Relaxed);
        if settings.watch_root && root != 0 {
            let word = Self::latch_word(root);
            ffs::log(&format!("[harness] watching root latch {word:#x}"));
            ffs::watch(word, 8);
        }
        if settings.watch_last && last != 0 && last != root {
            let word = Self::latch_word(last);
            ffs::log(&format!(
                "[harness] watching last page's latch {word:#x} (page {} of {})",
                self.count.load(Ordering::Relaxed),
                self.count.load(Ordering::Relaxed)
            ));
            ffs::watch(word, 8);
        }
    }
}

// SAFETY: every block comes from `Global` and goes back to it; this only
// takes note of the addresses.
unsafe impl Allocator for Pages {
    fn allocate(&self, layout: Layout) -> Result<NonNull<u8>, AllocError> {
        let page = Global.allocate(layout)?;
        let address = page.as_ptr() as usize;
        let _ = self
            .root
            .compare_exchange(0, address, Ordering::Relaxed, Ordering::Relaxed);
        self.last.store(address, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        Ok(page)
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: the caller's contract, which is `Global`'s.
        unsafe { Global.deallocate(ptr, layout) }
    }
}

/// Where a test's race begins: the watchpoints armed, then the checkpoint
/// every branch of `ffs explore` starts from.
struct Race;

impl Race {
    fn start(pages: &Pages) {
        let settings = Settings::from_env();
        pages.watch(&settings);
        ffs::checkpoint("race");
    }
}

/// Whether the map dropped each value a test made: a flag a value, in an
/// arena that outlives the map, so that a value read through a stale copy
/// of its bytes after its drop still finds its flag.
struct DropFlags {
    flags: Vec<AtomicBool>,
    next: AtomicUsize,
}

impl DropFlags {
    fn new() -> Self {
        Self {
            flags: (0..4_096).map(|_| AtomicBool::new(false)).collect(),
            next: AtomicUsize::new(0),
        }
    }

    /// A new value for the key numbered `id`.
    fn value(&self, id: u64) -> Tracked<'_> {
        let slot = self.next.fetch_add(1, Ordering::Relaxed);
        assert!(slot < self.flags.len(), "a test makes too many values");
        Tracked {
            id,
            slot,
            flags: self,
            _wide: [Tracked::FILL; 5],
        }
    }
}

/// A map's value: the number of its key, to tell whose entry it is, and its
/// drop flag in [`DropFlags`]. Wide, so that a leaf holds a dozen, and
/// filled with words that crash a thread that follows one as a node
/// pointer.
struct Tracked<'f> {
    id: u64,
    slot: usize,
    flags: &'f DropFlags,
    _wide: [u64; 5],
}

impl Tracked<'_> {
    /// Not null, not aligned, and not a mapped address.
    const FILL: u64 = 0xDEAD_0000_0000_0BAD;

    /// Panics unless this is the value of the key numbered `id` and the map
    /// has not dropped it, after a scheduling point: a closure that runs on
    /// an entry whose leaf its thread has not latched lets another thread
    /// remove and drop it.
    fn check(&self, id: u64) {
        ffs::yield_now();
        assert_eq!(self.id, id, "a closure ran on another key's value");
        assert!(
            !self.flags.flags[self.slot].load(Ordering::Relaxed),
            "a closure ran on key {id}'s value after the map dropped it"
        );
    }

    /// Which value this is: the same for every look at one entry.
    fn slot(&self) -> usize {
        self.slot
    }
}

impl fmt::Debug for Tracked<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Tracked({}, #{})", self.id, self.slot)
    }
}

impl Clone for Tracked<'_> {
    fn clone(&self) -> Self {
        self.flags.value(self.id)
    }
}

impl Drop for Tracked<'_> {
    fn drop(&mut self) {
        self.flags.flags[self.slot].store(true, Ordering::Relaxed);
    }
}

/// What a test owns besides its map: the values' drop flags and the pages
/// the map allocates from.
struct Fixture {
    flags: DropFlags,
    pages: Pages,
}

impl Fixture {
    fn new() -> Self {
        Self {
            flags: DropFlags::new(),
            pages: Pages::new(),
        }
    }

    fn value(&self, id: u64) -> Tracked<'_> {
        self.flags.value(id)
    }

    fn map<K>(&self) -> TrackedMap<'_, K> {
        BBTreeMap::new_in(&self.pages)
    }

    /// Marks the exploration root.
    fn race(&self) {
        Race::start(&self.pages);
    }
}

type TrackedMap<'f, K = u64> = BBTreeMap<K, Tracked<'f>, Auto, &'f Pages>;

/// The keys tests set trees up with, numbered: `key(0)`, `key(1)`, …,
/// multiples of 10, so that threads can insert keys between them.
fn key(index: u64) -> u64 {
    10 * (index + 1)
}

/// A map of the keys `key(0)`, …, `key(len - 1)`, inserted in order, each
/// with a value numbered by its key.
fn map_of(fx: &Fixture, len: u64) -> TrackedMap<'_> {
    let map = fx.map();
    for index in 0..len {
        assert!(map.insert(key(index), fx.value(key(index))).is_none());
    }
    map
}

/// The shapes tests start from, each one operation away from a change of
/// the tree's height: a tree of one or two levels, full along its right
/// border, which the next append raises; and a tree of two or three levels,
/// which the next removal of its last key lowers. Built with the keys
/// `key(0)`, … in order, and the last ones removed.
///
/// Found once a test, by building them: they follow from the node sizes
/// and how nodes split and merge, which this need not know.
struct Shapes {
    /// At index `levels - 1`: how many keys a tree of `levels` levels holds
    /// before the next append raises it.
    capacity: [u64; 2],
    /// At index `levels - 1`: how many keys are left of the
    /// `capacity[levels - 1] + 1` that raised the tree past `levels` levels
    /// when, removing keys from the end, the next removal lowers it back.
    lowering_len: [u64; 2],
    /// The most keys an internal node holds: the full root of two levels.
    internal_capacity: u64,
}

impl Shapes {
    fn probe() -> Self {
        let fx = Fixture::new();
        let mut capacity = [0; 2];
        let mut lowering_len = [0; 2];
        for levels in 0..2 {
            let mut map = fx.map::<u64>();
            let mut len = 0;
            while map.node_lengths().len() <= levels + 1 {
                map.insert(key(len), fx.value(key(len)));
                len += 1;
            }
            capacity[levels] = len - 1;
            while map.node_lengths().len() > levels + 1 {
                len -= 1;
                map.remove(&key(len));
            }
            lowering_len[levels] = len + 1;
        }
        let internal_capacity = map_of(&fx, capacity[1]).node_lengths()[0][0] as u64;
        Self {
            capacity,
            lowering_len,
            internal_capacity,
        }
    }

    /// A tree of `levels` levels, full along its right border: the next
    /// insert of a key past its last raises it. Returns how many keys it
    /// holds.
    fn full<'f>(&self, fx: &'f Fixture, levels: usize) -> (TrackedMap<'f>, u64) {
        let len = self.capacity[levels - 1];
        (map_of(fx, len), len)
    }

    /// A tree of `levels + 1` levels, which a removal of its last key lowers
    /// to `levels`. Returns how many keys it holds.
    fn one_removal_from_lowering<'f>(&self, fx: &'f Fixture, levels: usize) -> (TrackedMap<'f>, u64) {
        let (len, raised) = (self.lowering_len[levels - 1], self.capacity[levels - 1] + 1);
        let map = map_of(fx, raised);
        for index in len..raised {
            assert!(map.remove(&key(index)).is_some());
        }
        (map, len)
    }
}

/// Visits `range` of the map with `scan`, or `scan_rev` if `reverse`,
/// checking each value against `id_of` its key, and returns the keys
/// visited, in key order.
fn scanned_keys<K: Ord + Clone>(
    map: &TrackedMap<'_, K>,
    range: impl RangeBounds<K>,
    id_of: fn(&K) -> u64,
    reverse: bool,
) -> Vec<K> {
    let mut keys = Vec::new();
    let visit = |key: &K, value: &Tracked<'_>| {
        value.check(id_of(key));
        keys.push(key.clone());
        ControlFlow::<()>::Continue(())
    };
    if reverse {
        map.scan_rev::<K, ()>(range, visit);
        keys.reverse();
    } else {
        map.scan::<K, ()>(range, visit);
    }
    keys
}

/// Panics unless `visited` is in strictly increasing key order and holds
/// every key of `steady`.
fn assert_visited_in_order<K: Ord + fmt::Debug>(visited: &[K], steady: &[K]) {
    assert!(
        visited.is_sorted_by(|a, b| a < b),
        "visited out of order, or twice: {visited:?}"
    );
    for key in steady {
        assert!(
            visited.binary_search(key).is_ok(),
            "missed {key:?}: {visited:?}"
        );
    }
}

/// The number of a `u64` key, its value's.
fn same(key: &u64) -> u64 {
    *key
}

// Small tests: one change, two or three threads, a few operations each.

/// Scans from either end, and a cloned range's first steps, while the tree
/// lowers from two leaves into its root leaf: a scan that read the root as
/// internal must not take the leaf's bytes for edges.
#[test]
fn scans_race_the_root_lowering_to_a_leaf() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let (mut map, len) = shapes.one_removal_from_lowering(&fx, 1);
    let steady: Vec<u64> = (0..len - 1).map(key).collect();
    fx.race();
    thread::scope(|scope| {
        scope.spawn(|| assert!(map.remove(&key(len - 1)).is_some()));
        scope.spawn(|| {
            assert_visited_in_order(&scanned_keys(&map, .., same, false), &steady);
            assert_visited_in_order(&scanned_keys(&map, .., same, true), &steady);
            let mut range = map.range_cloned::<u64, _>(..);
            let (first, value) = range.next().expect("steady keys");
            value.check(first);
            assert_eq!(first, key(0));
            let (last, value) = range.next_back().expect("steady keys");
            value.check(last);
            assert!(last >= key(len - 2));
        });
    });
    assert_eq!(map.node_lengths().len(), 1, "the root lowered to a leaf");
    map.check_invariants(steady.len());
}

/// A batched lookup of a separator key while its leaf merges into its left
/// sibling, freed, and the key is removed: the lookup must not find the
/// entry in the freed leaf's stale bytes.
#[test]
fn batched_separator_lookups_race_a_merge() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let (mut map, len) = shapes.one_removal_from_lowering(&fx, 1);
    let separator = map.leaf_keys()[1][0];
    fx.race();
    thread::scope(|scope| {
        scope.spawn(|| {
            let found = map.read_many([&separator, &key(0)], |key, value| {
                value.check(*key);
                *key
            });
            assert_eq!(found[1], Some(key(0)));
        });
        scope.spawn(|| {
            assert!(map.remove(&key(len - 1)).is_some());
            assert!(map.remove(&separator).is_some());
        });
    });
    assert_eq!(map.node_lengths().len(), 1, "the root lowered to a leaf");
    map.check_invariants(len as usize - 2);
}

/// A lookup of a separator key while its leaf merges into its left sibling,
/// freed, and the key is removed: the lookup must not find the entry in the
/// freed leaf's stale bytes.
#[test]
fn separator_lookups_race_a_merge() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let (mut map, len) = shapes.one_removal_from_lowering(&fx, 1);
    let separator = map.leaf_keys()[1][0];
    fx.race();
    thread::scope(|scope| {
        scope.spawn(|| {
            map.read(&separator, |key, value| value.check(*key));
        });
        scope.spawn(|| {
            assert!(map.remove(&key(len - 1)).is_some());
            assert!(map.remove(&separator).is_some());
        });
    });
    assert_eq!(map.node_lengths().len(), 1, "the root lowered to a leaf");
    map.check_invariants(len as usize - 2);
}

/// Lookups, batched and not, while a full root leaf splits into an internal
/// root, and the tree lowers back to a leaf: every descent reads the root as
/// it is.
#[test]
fn lookups_race_the_root_raised_and_lowered() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let (mut map, len) = shapes.full(&fx, 1);
    let (first, last) = (key(0), key(len - 1));
    let added = last - 5;
    // With `added` removed, the tree is the one `Shapes` lowered by
    // removing keys from the end.
    let lowering_len = shapes.lowering_len[0];
    fx.race();
    thread::scope(|scope| {
        scope.spawn(|| {
            assert!(map.insert(added, fx.value(added)).is_none());
            assert!(map.remove(&added).is_some());
            for index in (lowering_len - 1..len).rev() {
                assert!(map.remove(&key(index)).is_some());
            }
        });
        scope.spawn(|| {
            let found = map.read_many([&first, &added], |key, value| {
                value.check(*key);
                *key
            });
            assert_eq!(found[0], Some(first));
            map.read(&added, |_, value| value.check(added));
            map.read(&last, |_, value| value.check(last));
        });
    });
    assert_eq!(map.node_lengths().len(), 1, "the root lowered to a leaf");
    map.check_invariants(lowering_len as usize - 1);
}

/// Removing a separator, its successor taking its place, and then the
/// successor, which merges the leaf, while another thread reads the keys
/// around them and scans across them.
#[test]
fn removing_separators_races_lookups_around_them() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let (mut map, len) = shapes.one_removal_from_lowering(&fx, 1);
    let right = map.leaf_keys()[1].clone();
    let (separator, successor) = (right[0], right[1]);
    let before = separator - 10;
    let steady: Vec<u64> = (0..len)
        .map(key)
        .filter(|&key| key != separator && key != successor)
        .collect();
    fx.race();
    thread::scope(|scope| {
        scope.spawn(|| {
            assert!(map.remove(&separator).is_some());
            assert!(map.remove(&successor).is_some());
        });
        scope.spawn(|| {
            map.read(&successor, |_, value| value.check(successor));
            let updated = map.update(&before, |_, value| value.check(before));
            assert!(updated.is_some());
            let mut visited = Vec::new();
            map.scan::<u64, ()>(before.., |key, value| {
                value.check(*key);
                visited.push(*key);
                ControlFlow::Continue(())
            });
            let after: Vec<u64> = steady.iter().copied().filter(|&k| k >= before).collect();
            assert_visited_in_order(&visited, &after);
        });
    });
    assert_eq!(map.node_lengths().len(), 1, "the root lowered to a leaf");
    map.check_invariants(steady.len());
}

/// An append to the last leaf, which splits it, while another thread
/// inserts a key that lacks what every key shares, which shrinks the shared
/// prefix along both borders, and a third reads the first key, on the left
/// border.
#[test]
fn appends_race_a_shrinking_shared_prefix() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let base = 0xAB << 40;
    let len = 2 * shapes.capacity[0];
    let mut map = fx.map();
    for index in 0..len {
        map.insert(base + key(index), fx.value(base + key(index)));
    }
    assert!(map.shared_prefix_len() >= 3);
    let (appended, outside, first) = (base + key(len), 5, base + key(0));
    fx.race();
    thread::scope(|scope| {
        scope.spawn(|| assert!(map.insert(appended, fx.value(appended)).is_none()));
        scope.spawn(|| assert!(map.insert(outside, fx.value(outside)).is_none()));
        scope.spawn(|| {
            assert!(map.read(&first, |_, value| value.check(first)).is_some());
            map.read(&outside, |_, value| value.check(outside));
        });
    });
    map.check_invariants(len as usize + 2);
    assert!(map.shared_prefix_len() < 3);
    for key in [appended, outside, first] {
        assert!(map.read(&key, |_, value| value.check(key)).is_some());
    }
}

/// Appends while a removal merges the last leaf into its sibling, frees it,
/// and lowers the tree into the root leaf: an append that read the pointer
/// to the last leaf before must not insert into the freed page.
#[test]
fn appends_race_a_merge_that_frees_the_last_leaf() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let (mut map, len) = shapes.one_removal_from_lowering(&fx, 1);
    let appended = [key(len), key(len + 1)];
    fx.race();
    thread::scope(|scope| {
        scope.spawn(|| assert!(map.remove(&key(len - 1)).is_some()));
        for key in appended {
            let (map, fx) = (&map, &fx);
            scope.spawn(move || assert!(map.insert(key, fx.value(key)).is_none()));
        }
    });
    map.check_invariants(len as usize + 1);
    for key in appended {
        assert!(map.read(&key, |_, value| value.check(key)).is_some());
    }
}

/// Two threads `get_or_insert` the same key into a full root leaf, which
/// splits, while a third removes another key under a condition: both see
/// the one entry the map keeps.
#[test]
fn racing_get_or_inserts_see_one_entry() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let (mut map, len) = shapes.full(&fx, 1);
    let contested = key(3) + 5;
    let get_or_insert = || {
        map.get_or_insert(contested, fx.value(contested), |_, value| {
            value.check(contested);
            value.slot()
        })
    };
    fx.race();
    let (first, second) = thread::scope(|scope| {
        let first = scope.spawn(get_or_insert);
        let second = scope.spawn(get_or_insert);
        scope.spawn(|| {
            let removed = map.remove_if(&key(0), |_, value| {
                value.check(key(0));
                true
            });
            assert!(removed.is_some());
        });
        (first.join().unwrap(), second.join().unwrap())
    });
    assert_eq!(first, second, "both saw the entry the map keeps");
    map.check_invariants(len as usize);
}

/// A cloned range copies entries ahead, and resumes its scan where the
/// last copy stopped, while another thread merges the leaf it stopped in,
/// which frees its page, and splits the root leaf again, which takes the
/// page back: the range hands out each steady key once, in order.
#[test]
fn cloned_ranges_resume_across_a_freed_and_reused_leaf() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let (mut map, len) = shapes.one_removal_from_lowering(&fx, 1);
    let steady: Vec<u64> = (0..len - 1).map(key).collect();
    // Enough keys, between the steady ones, to split the root leaf.
    let added: Vec<u64> = (0..shapes.capacity[0] + 2 - len)
        .map(|index| key(index) + 5)
        .collect();
    fx.race();
    thread::scope(|scope| {
        scope.spawn(|| {
            assert!(map.remove(&key(len - 1)).is_some());
            for &key in &added {
                assert!(map.insert(key, fx.value(key)).is_none());
            }
        });
        scope.spawn(|| {
            let handed_out: Vec<u64> = (map.range_cloned::<u64, _>(..))
                .map(|(key, value)| {
                    value.check(key);
                    key
                })
                .collect();
            assert_visited_in_order(&handed_out, &steady);
        });
    });
    assert_eq!(map.node_lengths().len(), 2, "the root leaf split again");
    map.check_invariants(steady.len() + added.len());
}

/// Two splits of full leaves under a full root, each of which splits the
/// root and raises the tree to three levels, the first to take the root,
/// while a third thread scans and looks up keys.
#[test]
fn splits_race_to_raise_a_full_internal_root() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let (mut map, len) = shapes.full(&fx, 2);
    // The first leaf filled too, with keys between its first ones.
    let first_leaf = map.leaf_keys()[0].len() as u64;
    let filled: Vec<u64> = (0..shapes.capacity[0] - first_leaf)
        .map(|index| key(index) + 5)
        .collect();
    for &key in &filled {
        map.insert(key, fx.value(key));
    }
    let (appended, between) = (key(len), key(0) + 1);
    fx.race();
    thread::scope(|scope| {
        scope.spawn(|| assert!(map.insert(appended, fx.value(appended)).is_none()));
        scope.spawn(|| assert!(map.insert(between, fx.value(between)).is_none()));
        scope.spawn(|| {
            let steady: Vec<u64> = (0..len).map(key).collect();
            assert_visited_in_order(&scanned_keys(&map, .., same, true), &steady);
            let last = key(len - 1);
            let found = map.read_many([&last, &key(0)], |key, value| {
                value.check(*key);
                *key
            });
            assert_eq!(found, [Some(last), Some(key(0))]);
        });
    });
    assert_eq!(map.node_lengths().len(), 3, "the tree has three levels");
    map.check_invariants(len as usize + filled.len() + 2);
}

/// Lookups of the root's separator key, which they match in the root,
/// while another thread removes it, its successor taking its place as the
/// separator and its leaf's head: a descent that matched it must find it,
/// or nothing, not the successor.
#[test]
fn lookups_race_removing_the_roots_separator() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let (mut map, len) = shapes.one_removal_from_lowering(&fx, 2);
    let separator = map.leaf_keys()[map.node_lengths()[1][0] + 1][0];
    fx.race();
    thread::scope(|scope| {
        scope.spawn(|| assert!(map.remove(&separator).is_some()));
        scope.spawn(|| {
            map.read(&separator, |_, value| value.check(separator));
            map.update(&separator, |_, value| value.check(separator));
        });
    });
    assert_eq!(
        map.node_lengths().len(),
        3,
        "the separator's leaf kept its minimum"
    );
    map.check_invariants(len as usize - 1);
}

/// A removal that merges leaves, then internal nodes, and lowers the tree
/// from three levels to two, while threads scan, look up and insert across
/// the root's separator.
#[test]
fn lookups_race_three_levels_lowering_to_two() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let (mut map, len) = shapes.one_removal_from_lowering(&fx, 2);
    let separator = map.leaf_keys()[map.node_lengths()[1][0] + 1][0];
    let (before, inserted) = (separator - 10, separator + 5);
    let steady: Vec<u64> = (0..len - 1).map(key).collect();
    fx.race();
    thread::scope(|scope| {
        scope.spawn(|| assert!(map.remove(&key(len - 1)).is_some()));
        scope.spawn(|| assert!(map.insert(inserted, fx.value(inserted)).is_none()));
        scope.spawn(|| {
            let found = map.read_many([&before, &separator], |key, value| {
                value.check(*key);
                *key
            });
            assert_eq!(found, [Some(before), Some(separator)]);
            assert_visited_in_order(&scanned_keys(&map, .., same, false), &steady);
        });
    });
    assert_eq!(
        map.node_lengths().len(),
        2,
        "the tree lowered to two levels"
    );
    map.check_invariants(steady.len() + 1);
}

/// Inserts into an empty map, each of which makes the tree unless another
/// thread did, one of them enough keys to split the root leaf on its own:
/// one tree, with every key, whose appends go to its last leaf. Two threads
/// only: a third holding the leaf shared would make the splitting one
/// wait, and yield to the one that made the tree.
#[test]
fn inserts_race_to_start_the_tree() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let mut map = fx.map();
    let len = shapes.capacity[0] + 2;
    let low = key(0);
    fx.race();
    thread::scope(|scope| {
        scope.spawn(|| {
            let slot = map.get_or_insert(low, fx.value(low), |_, value| {
                value.check(low);
                value.slot()
            });
            map.read(&low, |_, value| assert_eq!(value.slot(), slot));
            let found = map.read_many([&low, &key(len - 1)], |key, value| {
                value.check(*key);
                *key
            });
            assert_eq!(found[0], Some(low));
            assert_visited_in_order(&scanned_keys(&map, .., same, true), &[low]);
        });
        scope.spawn(|| {
            for index in 1..len {
                assert!(map.insert(key(index), fx.value(key(index))).is_none());
            }
        });
    });
    assert_eq!(map.node_lengths().len(), 2, "the root leaf split");
    map.check_invariants(len as usize);
}

/// The last key removed, which empties the tree, while threads insert a key
/// that lacks what the removed key shared with the tree, and one that shares
/// it: what every key shares starts anew from the keys left, or shrinks.
#[test]
fn emptying_the_tree_races_inserts() {
    let fx = Fixture::new();
    let mut map = map_of(&fx, 1);
    let (outside, sharing) = (1 << 40, key(1));
    fx.race();
    thread::scope(|scope| {
        scope.spawn(|| assert!(map.remove(&key(0)).is_some()));
        scope.spawn(|| assert!(map.insert(outside, fx.value(outside)).is_none()));
        scope.spawn(|| {
            assert!(map.insert(sharing, fx.value(sharing)).is_none());
            map.read(&key(0), |_, value| value.check(key(0)));
        });
    });
    map.check_invariants(2);
    for key in [outside, sharing] {
        assert!(map.read(&key, |_, value| value.check(key)).is_some());
    }
}

/// Threads race to remove the same key, from a leaf a removal merges, one
/// with `remove`, one with `remove_entry` and one with `remove_if`: exactly
/// one takes it.
#[test]
fn racing_removals_take_an_entry_once() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let (mut map, len) = shapes.one_removal_from_lowering(&fx, 1);
    let contested = key(len - 1);
    fx.race();
    let taken = thread::scope(|scope| {
        let removals = [
            scope.spawn(|| map.remove(&contested).map(|value| value.slot())),
            scope.spawn(|| map.remove_entry(&contested).map(|(_, value)| value.slot())),
            scope.spawn(|| {
                let removed = map.remove_if(&contested, |_, value| {
                    value.check(contested);
                    true
                });
                removed.map(|(_, value)| value.slot())
            }),
        ];
        removals.map(|removal| removal.join().unwrap())
    });
    assert_eq!(taken.iter().flatten().count(), 1, "taken once: {taken:?}");
    assert_eq!(map.node_lengths().len(), 1, "the root lowered to a leaf");
    map.check_invariants(len as usize - 1);
}

/// A removal from a leaf at its minimum, whose left sibling is full: the
/// leaf takes entries from it, and the separator between them moves, while
/// threads look up the moving entries and the separator, scan across them,
/// and update the leaf that gives.
#[test]
fn removals_take_entries_from_a_full_sibling_leaf() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let (mut map, len) = shapes.one_removal_from_lowering(&fx, 1);
    let left_len = map.leaf_keys()[0].len() as u64;
    // The left leaf filled, with keys between its own.
    for index in 0..shapes.capacity[0] - left_len {
        map.insert(key(index) + 5, fx.value(key(index) + 5));
    }
    let leaves = map.leaf_keys();
    let (giving, separator) = (*leaves[0].last().expect("a full leaf"), leaves[1][0]);
    let steady: Vec<u64> = leaves
        .concat()
        .into_iter()
        .filter(|&k| k != key(len - 1))
        .collect();
    fx.race();
    thread::scope(|scope| {
        scope.spawn(|| assert!(map.remove(&key(len - 1)).is_some()));
        scope.spawn(|| {
            let found = map.read_many([&giving, &separator], |key, value| {
                value.check(*key);
                *key
            });
            assert_eq!(found, [Some(giving), Some(separator)]);
            assert_visited_in_order(&scanned_keys(&map, .., same, true), &steady);
        });
        scope.spawn(|| {
            assert!(map.update(&giving, |_, value| value.check(giving)).is_some());
            assert!(map.read(&separator, |_, value| value.check(separator)).is_some());
        });
    });
    assert_eq!(map.node_lengths().len(), 2, "the leaves did not merge");
    map.check_invariants(steady.len());
}

/// A removal that merges two leaves under the root's last child, which has
/// its minimum of children, while its sibling has too many to merge with:
/// the last child takes a child from it, through the root, while threads
/// look up and insert keys in the child that moves, and scan across it.
#[test]
fn internal_nodes_take_a_child_from_a_full_sibling() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let (mut map, len) = shapes.one_removal_from_lowering(&fx, 2);
    let right_len = map.node_lengths()[1][1] as u64;
    // The left child given too many children to merge with the right,
    // by keys inserted between its first ones.
    let too_many = shapes.internal_capacity - right_len + 1;
    let mut added = 0;
    'filling: for index in 0.. {
        for offset in 1..4 {
            if map.node_lengths()[1][0] as u64 >= too_many {
                break 'filling;
            }
            map.insert(key(index) + offset, fx.value(key(index) + offset));
            added += 1;
        }
    }
    let leaves = map.leaf_keys();
    let lengths = map.node_lengths();
    let moving = leaves[lengths[1][0]].clone();
    let separator = leaves[lengths[1][0] + 1][0];
    let inserted = moving[0] + 5;
    let steady: Vec<u64> = leaves
        .concat()
        .into_iter()
        .filter(|&k| k != key(len - 1))
        .collect();
    fx.race();
    thread::scope(|scope| {
        scope.spawn(|| assert!(map.remove(&key(len - 1)).is_some()));
        scope.spawn(|| assert!(map.insert(inserted, fx.value(inserted)).is_none()));
        scope.spawn(|| {
            let found = map.read_many([&moving[0], &separator], |key, value| {
                value.check(*key);
                *key
            });
            assert_eq!(found, [Some(moving[0]), Some(separator)]);
            let visited = scanned_keys(&map, moving[0].., same, false);
            let after: Vec<u64> = steady.iter().copied().filter(|&k| k >= moving[0]).collect();
            assert_visited_in_order(&visited, &after);
        });
    });
    let lengths = map.node_lengths();
    assert_eq!(lengths.len(), 3, "the internal nodes did not merge");
    assert_eq!(lengths[1][1] as u64, right_len, "the right one took a child");
    map.check_invariants(len as usize + added);
}

/// Removing the root's separator from a leaf at its minimum, under the
/// root's last child, also at its minimum: the leaf merges, then the
/// internal nodes, and the tree lowers to two levels, in one removal. Other
/// threads insert a key between the separator and its successor, which must
/// go where the separator's successor, not the separator, now bounds it,
/// look up keys on both sides, and scan across. An insert into the leaf
/// first keeps it from merging: some schedules must lower the tree (logged;
/// Shuttle's version counts them across schedules).
#[test]
fn removing_the_roots_separator_lowers_three_levels() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let (mut map, len) = shapes.one_removal_from_lowering(&fx, 2);
    let lengths = map.node_lengths();
    let separator_leaf = map.leaf_keys()[lengths[1][0] + 1].clone();
    // Down to its minimum, the last leaf's, by removing its last keys:
    // it keeps its head and that many.
    let kept = lengths[2].last().copied().expect("leaves") + 1;
    let trimmed = separator_leaf.len() - kept;
    for key in &separator_leaf[kept..] {
        assert!(map.remove(key).is_some());
    }
    let (separator, successor) = (separator_leaf[0], separator_leaf[1]);
    let (before, between) = (separator - 10, separator + 5);
    let steady: Vec<u64> = (0..len)
        .map(key)
        .filter(|key| *key != separator && !separator_leaf[kept..].contains(key))
        .collect();
    fx.race();
    thread::scope(|scope| {
        scope.spawn(|| assert!(map.remove(&separator).is_some()));
        scope.spawn(|| assert!(map.insert(between, fx.value(between)).is_none()));
        scope.spawn(|| {
            let found = map.read_many([&before, &successor], |key, value| {
                value.check(*key);
                *key
            });
            assert_eq!(found, [Some(before), Some(successor)]);
            let visited = scanned_keys(&map, before.., same, true);
            let after: Vec<u64> = steady.iter().copied().filter(|&k| k >= before).collect();
            assert_visited_in_order(&visited, &after);
        });
    });
    let levels = map.node_lengths().len();
    ffs::log(&format!("[harness] levels after the race: {levels}"));
    assert!(levels == 2 || levels == 3, "levels: {levels}");
    map.check_invariants(len as usize - trimmed);
}

/// A key that lacks what every key shares, past every key: its insert
/// shrinks the shared prefix along both borders of a full tree of two
/// levels, then splits the full last leaf and the full root, raising the
/// tree to three levels, while threads insert into the first leaf, on the
/// left border, and look up keys on both.
#[test]
fn shrinking_the_shared_prefix_races_raising_the_root() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let (mut map, len) = shapes.full(&fx, 2);
    let shared = map.shared_prefix_len();
    let (outside, first_leaf, last) = (1 << 40, key(0) + 5, key(len - 1));
    fx.race();
    thread::scope(|scope| {
        scope.spawn(|| assert!(map.insert(outside, fx.value(outside)).is_none()));
        scope.spawn(|| assert!(map.insert(first_leaf, fx.value(first_leaf)).is_none()));
        scope.spawn(|| {
            let found = map.read_many([&key(0), &last], |key, value| {
                value.check(*key);
                *key
            });
            assert_eq!(found, [Some(key(0)), Some(last)]);
            map.read(&outside, |_, value| value.check(outside));
        });
    });
    assert_eq!(map.node_lengths().len(), 3, "the tree has three levels");
    assert!(map.shared_prefix_len() < shared);
    map.check_invariants(len as usize + 2);
}

// Large tests: random operations against models.

/// A small deterministic generator (xorshift64*), so that every branch of
/// an exploration runs the same operations.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A number below `n`.
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// A thread that runs random operations on keys of its own, numbered, and
/// checks the map's answers against its model of them; and lookups, scans
/// and cloned ranges over the span of the map the test works in, which hand
/// out the keys it holds.
struct ModelThread<'m, 'f, K> {
    run: &'m ModelRun<'f, K>,
    /// The numbers of the keys this thread owns.
    ids: Vec<u64>,
    /// The values the map holds of them, by slot.
    model: BTreeMap<u64, usize>,
    rng: Rng,
}

impl<K: Ord + Clone + fmt::Debug> ModelThread<'_, '_, K> {
    /// Runs `operations` random operations, then checks every key it owns,
    /// and returns its model.
    fn run(mut self, operations: usize) -> BTreeMap<u64, usize> {
        for _ in 0..operations {
            let id = self.ids[self.rng.below(self.ids.len())];
            match self.rng.below(100) {
                0..25 => self.insert(id),
                25..45 => self.remove(id),
                45..52 => self.update(id),
                52..58 => self.remove_if(id),
                58..66 => self.read(id),
                66..76 => {
                    let other = self.ids[self.rng.below(self.ids.len())];
                    self.read_many(id, other);
                }
                76..85 => {
                    let reverse = self.rng.below(2) == 1;
                    self.scan(reverse);
                }
                85..93 => self.range_cloned(),
                _ => self.get_or_insert(id),
            }
        }
        for id in self.ids.clone() {
            self.read(id);
        }
        self.model
    }

    fn key(&self, id: u64) -> K {
        (self.run.key_of)(id)
    }

    fn insert(&mut self, id: u64) {
        let value = self.run.fx.value(id);
        let slot = value.slot();
        let old = self.run.map.insert(self.key(id), value);
        assert_eq!(old.map(|old| old.slot()), self.model.insert(id, slot));
    }

    fn remove(&mut self, id: u64) {
        let removed = self.run.map.remove(&self.key(id));
        assert_eq!(removed.map(|old| old.slot()), self.model.remove(&id));
    }

    /// Replaces the value in place, which drops the old one while the leaf
    /// is latched exclusively.
    fn update(&mut self, id: u64) {
        let value = self.run.fx.value(id);
        let slot = value.slot();
        let replaced = self.run.map.update(&self.key(id), |_, old| {
            old.check(id);
            mem::replace(old, value).slot()
        });
        let modeled = self.model.get_mut(&id).map(|old| mem::replace(old, slot));
        assert_eq!(replaced, modeled);
    }

    /// Removes the entry if its value's slot is even.
    fn remove_if(&mut self, id: u64) {
        let removed = self.run.map.remove_if(&self.key(id), |_, value| {
            value.check(id);
            value.slot() % 2 == 0
        });
        let modeled = (self.model.get(&id))
            .filter(|&&slot| slot % 2 == 0)
            .copied();
        if modeled.is_some() {
            self.model.remove(&id);
        }
        assert_eq!(removed.map(|(_, value)| value.slot()), modeled);
    }

    fn read(&self, id: u64) {
        let slot = self.run.map.read(&self.key(id), |_, value| {
            value.check(id);
            value.slot()
        });
        assert_eq!(slot.as_ref(), self.model.get(&id));
    }

    /// A batch of two keys of its own and the key numbered after the
    /// first, which another thread owns or no key has.
    fn read_many(&self, id: u64, other: u64) {
        let keys = [self.key(id), self.key(other), self.key(id + 1)];
        let slots = self
            .run
            .map
            .read_many([&keys[0], &keys[1], &keys[2]], |key, value| {
                value.check((self.run.id_of)(key));
                value.slot()
            });
        assert_eq!(slots[0].as_ref(), self.model.get(&id));
        assert_eq!(slots[1].as_ref(), self.model.get(&other));
    }

    fn get_or_insert(&mut self, id: u64) {
        let value = self.run.fx.value(id);
        let slot = value.slot();
        let seen = self.run.map.get_or_insert(self.key(id), value, |_, value| {
            value.check(id);
            value.slot()
        });
        assert_eq!(seen, *self.model.entry(id).or_insert(slot));
    }

    /// A scan of the span, in either direction.
    fn scan(&self, reverse: bool) {
        let visited = scanned_keys(
            &self.run.map,
            self.run.span.clone(),
            self.run.id_of,
            reverse,
        );
        self.assert_hands_out_its_keys(&visited);
    }

    /// A cloned range over the span, taken from either end at random until
    /// the ends meet.
    fn range_cloned(&mut self) {
        let mut range = self.run.map.range_cloned::<K, _>(self.run.span.clone());
        let (mut front, mut back) = (Vec::new(), Vec::new());
        loop {
            let from_front = self.rng.below(2) == 0;
            let next = if from_front {
                range.next()
            } else {
                range.next_back()
            };
            let Some((key, value)) = next else {
                break;
            };
            value.check((self.run.id_of)(&key));
            if from_front {
                front.push(key);
            } else {
                back.push(key);
            }
        }
        front.extend(back.into_iter().rev());
        self.assert_hands_out_its_keys(&front);
    }

    /// Panics unless `handed_out`, from a scan or a cloned range over the
    /// span, is in order, once each, with every key the model holds and
    /// none of this thread's that it lacks.
    fn assert_hands_out_its_keys(&self, handed_out: &[K]) {
        let held: Vec<K> = self.model.keys().map(|&id| self.key(id)).collect();
        assert_visited_in_order(handed_out, &held);
        for &id in &self.ids {
            if !self.model.contains_key(&id) {
                let key = self.key(id);
                assert!(
                    handed_out.binary_search(&key).is_err(),
                    "handed out removed {key:?}"
                );
            }
        }
    }
}

/// A map shared by model threads, how to tell its keys by number, and the
/// span of it the threads' keys lie in, which their scans cover.
struct ModelRun<'f, K> {
    map: TrackedMap<'f, K>,
    fx: &'f Fixture,
    key_of: fn(u64) -> K,
    id_of: fn(&K) -> u64,
    span: (Bound<K>, Bound<K>),
}

impl<'f, K: Ord + Clone + fmt::Debug + Send + Sync> ModelRun<'f, K> {
    /// Runs a thread for each of `owned`, the numbers of the keys it owns,
    /// with `operations` random operations (or `FFS_BBTREE_OPS`); then
    /// checks that the map holds what the threads' models do and the keys
    /// no thread owns, in a well-formed tree.
    fn run(mut self, owned: Vec<Vec<u64>>, operations: usize) {
        let settings = Settings::from_env();
        let operations = settings.operations.unwrap_or(operations);
        let all = scanned_keys(&self.map, .., self.id_of, false);
        let owned_ids: BTreeSet<u64> = owned.iter().flatten().copied().collect();
        let mut expected: BTreeSet<u64> = (all.iter().map(self.id_of))
            .filter(|id| !owned_ids.contains(id))
            .collect();
        let threads: Vec<ModelThread<'_, 'f, K>> = (owned.into_iter().enumerate())
            .map(|(index, ids)| ModelThread {
                run: &self,
                model: self.model_of(&ids),
                ids,
                rng: Rng::new(settings.seed * 1000 + index as u64),
            })
            .collect();
        self.fx.race();
        let models: Vec<BTreeMap<u64, usize>> = thread::scope(|scope| {
            let handles: Vec<_> = (threads.into_iter())
                .map(|thread| scope.spawn(move || thread.run(operations)))
                .collect();
            (handles.into_iter())
                .map(|handle| handle.join().unwrap())
                .collect()
        });
        expected.extend(models.iter().flat_map(|model| model.keys().copied()));
        let held: BTreeSet<u64> = (scanned_keys(&self.map, .., self.id_of, false).iter())
            .map(self.id_of)
            .collect();
        assert_eq!(held, expected);
        self.map.check_invariants(held.len());
    }

    /// What the map holds of the keys numbered `ids`, by slot.
    fn model_of(&self, ids: &[u64]) -> BTreeMap<u64, usize> {
        (ids.iter())
            .filter_map(|&id| {
                let slot = self.map.read(&(self.key_of)(id), |_, value| value.slot());
                slot.map(|slot| (id, slot))
            })
            .collect()
    }
}

/// `ids` dealt out to `threads` threads in turn.
fn dealt(ids: impl Iterator<Item = u64>, threads: usize) -> Vec<Vec<u64>> {
    let mut owned = std::vec![Vec::new(); threads];
    for (turn, id) in ids.enumerate() {
        owned[turn % threads].push(id);
    }
    owned
}

/// Three threads on keys about a leaf and a half of entries: the root leaf
/// splits, the tree lowers back, and leaves split and merge, over and over,
/// under lookups and scans.
#[test]
fn threads_match_their_models_around_one_leaf() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let capacity = shapes.capacity[0];
    let map = map_of(&fx, capacity / 2);
    let owned = dealt((0..capacity + capacity / 2).map(key), 3);
    ModelRun {
        map,
        fx: &fx,
        key_of: |id| id,
        id_of: same,
        span: (Bound::Unbounded, Bound::Unbounded),
    }
    .run(owned, 12);
}

/// Three threads on the keys of the last leaves of a tree of three levels,
/// which a merge of leaves under the root's last child lowers to two:
/// leaves split and merge, the last child takes children from its sibling
/// or merges with it, and the tree lowers and grows back.
#[test]
fn threads_match_their_models_in_three_levels() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let (map, len) = shapes.one_removal_from_lowering(&fx, 2);
    // Three leaves' worth of keys from the end, and keys between them.
    let (first, last) = (key(len - 1) - 3 * shapes.capacity[0] / 2 * 10, key(len + 2));
    let owned = dealt((first..last).step_by(5), 3);
    ModelRun {
        map,
        fx: &fx,
        key_of: |id| id,
        id_of: same,
        span: (Bound::Included(first), Bound::Excluded(last)),
    }
    .run(owned, 12);
}

/// [`threads_match_their_models_around_one_leaf`] with string keys in two
/// runs that share their first bytes past what every key shares: internal
/// nodes keep 4-byte windows of them, which tie, and a descent compares the
/// whole key at the head of the leaf right of a tied separator, latched.
#[test]
fn string_keys_tie_at_separators_while_threads_change_them() {
    fn key_of(id: u64) -> String {
        format!("tie/{}/{id:06}", id % 2)
    }
    #[expect(clippy::ptr_arg, reason = "the map's keys are `String`s")]
    fn id_of(key: &String) -> u64 {
        key[key.len() - 6..].parse().expect("a numbered key")
    }
    let fx = Fixture::new();
    let map = fx.map::<String>();
    for id in 0..12 {
        map.insert(key_of(id), fx.value(id));
    }
    ModelRun {
        map,
        fx: &fx,
        key_of,
        id_of,
        span: (Bound::Unbounded, Bound::Unbounded),
    }
    .run(dealt(0..30, 3), 10);
}

/// The shapes this build has, for the log: how many keys raise and lower
/// the tree, and the pages' latch offset.
#[test]
fn shapes_are_as_the_tests_assume() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let (mut map, len) = shapes.one_removal_from_lowering(&fx, 1);
    let root = fx.pages.root.load(Ordering::Relaxed);
    let latch = Pages::latch_word(root);
    let line = format!(
        "[harness] capacity {:?}, lowering_len {:?}, internal_capacity {}, leaves {:?}, latch offset {}, under ffs: {}",
        shapes.capacity,
        shapes.lowering_len,
        shapes.internal_capacity,
        map.node_lengths(),
        latch - root,
        ffs::is_active()
    );
    std::eprintln!("{line}");
    ffs::log(&line);
    assert!(shapes.capacity[0] >= 8, "a leaf holds a handful of wide values");
    assert!(len > 2);
    let _ = line.to_string();
}

// More scenarios: paths the Shuttle file does not race.

/// A map of `Copy` values, for the lookups that copy a value out of a leaf
/// without its latch and check the leaf's version after (`get_copied`,
/// `get_many_copied`, `contains_key`; `doc/09-concurrency.md`, Lookups):
/// `Tracked` is not `Copy`, so the scenarios above never take that path.
type CopiedMap<'f> = BBTreeMap<u64, u64, Auto, &'f Pages>;

/// The value a key holds in a `CopiedMap`, and after an `update`: anything
/// else a lookup hands out is a torn or foreign copy.
fn copied_value(key: u64) -> u64 {
    key.rotate_left(32) ^ 0x5EED_0000_C0DE_0000
}

fn updated_value(key: u64) -> u64 {
    copied_value(key) ^ 1
}

/// Panics unless `found` is what `key` may hold: nothing, its value, or its
/// updated value if `updated`.
fn assert_copied(key: u64, found: Option<u64>, updated: bool) {
    match found {
        None => {}
        Some(v) if v == copied_value(key) => {}
        Some(v) if updated && v == updated_value(key) => {}
        Some(v) => panic!("key {key} copied as {v:#x}, neither {:#x} nor absent", copied_value(key)),
    }
}

/// [`Shapes`] for a `CopiedMap`, whose leaves hold 55 entries instead of a
/// dozen.
struct CopiedShapes {
    capacity: [u64; 2],
    lowering_len: [u64; 2],
}

impl CopiedShapes {
    fn probe() -> Self {
        let pages = Pages::new();
        let mut capacity = [0; 2];
        let mut lowering_len = [0; 2];
        for levels in 0..2 {
            let mut map: CopiedMap<'_> = BBTreeMap::new_in(&pages);
            let mut len = 0;
            while map.node_lengths().len() <= levels + 1 {
                map.insert(key(len), copied_value(key(len)));
                len += 1;
            }
            capacity[levels] = len - 1;
            while map.node_lengths().len() > levels + 1 {
                len -= 1;
                map.remove(&key(len));
            }
            lowering_len[levels] = len + 1;
        }
        Self {
            capacity,
            lowering_len,
        }
    }

    fn map_of<'f>(pages: &'f Pages, len: u64) -> CopiedMap<'f> {
        let map = BBTreeMap::new_in(pages);
        for index in 0..len {
            assert!(map.insert(key(index), copied_value(key(index))).is_none());
        }
        map
    }

    fn one_removal_from_lowering<'f>(&self, pages: &'f Pages, levels: usize) -> (CopiedMap<'f>, u64) {
        let (len, raised) = (self.lowering_len[levels - 1], self.capacity[levels - 1] + 1);
        let map = Self::map_of(pages, raised);
        for index in len..raised {
            assert!(map.remove(&key(index)).is_some());
        }
        (map, len)
    }
}

/// Latch-free copies of values while the tree lowers from two leaves into
/// its root leaf and is raised again, three times over: each copy is the
/// key's value or nothing, never bytes of a leaf being merged or split.
#[test]
fn copied_lookups_race_the_root_lowering_and_raising() {
    let shapes = CopiedShapes::probe();
    let pages = Pages::new();
    let (mut map, len) = shapes.one_removal_from_lowering(&pages, 1);
    let last = key(len - 1);
    let steady: Vec<u64> = (0..len - 1).map(key).collect();
    Race::start(&pages);
    thread::scope(|scope| {
        scope.spawn(|| {
            for _ in 0..3 {
                assert_eq!(map.remove(&last), Some(copied_value(last)));
                assert!(map.insert(last, copied_value(last)).is_none());
            }
        });
        scope.spawn(|| {
            for round in 0..4 {
                for &k in &steady {
                    assert_eq!(map.get_copied(&k), Some(copied_value(k)), "steady key {k}");
                    assert!(map.contains_key(&k));
                }
                assert_copied(last, map.get_copied(&last), false);
                let _ = map.contains_key(&last);
                let found = map.get_many_copied([&steady[0], &last, &steady[steady.len() - 1], &(last + 5)]);
                assert_eq!(found[0], Some(copied_value(steady[0])));
                assert_copied(last, found[1], false);
                assert_eq!(found[2], Some(copied_value(steady[steady.len() - 1])));
                assert_eq!(found[3], None, "round {round}");
            }
        });
    });
    assert_eq!(map.node_lengths().len(), 1, "the reinserted key fits the root leaf");
    map.check_invariants(len as usize);
}

/// Latch-free copies while a full root leaf splits and every value is then
/// updated in place: a copy is the old value, the new one, or nothing.
#[test]
fn copied_lookups_race_a_split_and_updates() {
    let shapes = CopiedShapes::probe();
    let pages = Pages::new();
    let len = shapes.capacity[0];
    let mut map = CopiedShapes::map_of(&pages, len);
    let between = key(len / 2) + 5;
    let keys: Vec<u64> = (0..len).map(key).collect();
    Race::start(&pages);
    thread::scope(|scope| {
        scope.spawn(|| {
            assert!(map.insert(between, copied_value(between)).is_none());
            for &k in &keys {
                let old = map.update(&k, |_, v| mem::replace(v, updated_value(k)));
                assert_eq!(old, Some(copied_value(k)));
            }
        });
        scope.spawn(|| {
            for _ in 0..3 {
                for &k in &keys {
                    let found = map.get_copied(&k);
                    assert!(found.is_some(), "steady key {k} missing");
                    assert_copied(k, found, true);
                }
                assert_copied(between, map.get_copied(&between), false);
                let found = map.get_many_copied([&keys[0], &between, &keys[keys.len() - 1]]);
                assert_copied(keys[0], found[0], true);
                assert_copied(between, found[1], false);
                assert_copied(keys[keys.len() - 1], found[2], true);
            }
        });
    });
    assert_eq!(map.node_lengths().len(), 2, "the root leaf split");
    for &k in &keys {
        assert_eq!(map.get_copied(&k), Some(updated_value(k)));
    }
    map.check_invariants(len as usize + 1);
}

/// Three threads insert the same key, into a full root leaf that splits,
/// three times each: every value they put in is returned to exactly one
/// later insert, except the one the map keeps, and the key is never lost.
#[test]
fn racing_inserts_of_one_key_return_each_old_value_once() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let (mut map, len) = shapes.full(&fx, 1);
    let contested = key(len / 2) + 5;
    let inserted = AtomicUsize::new(0);
    fx.race();
    let returned: Vec<Vec<usize>> = thread::scope(|scope| {
        let handles: Vec<_> = (0..3)
            .map(|_| {
                let (map, fx, inserted) = (&map, &fx, &inserted);
                scope.spawn(move || {
                    let mut returned = Vec::new();
                    for _ in 0..3 {
                        let value = fx.value(contested);
                        inserted.fetch_add(1, Ordering::Relaxed);
                        if let Some(old) = map.insert(contested, value) {
                            old.check(contested);
                            returned.push(old.slot());
                        }
                    }
                    returned
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut all: Vec<usize> = returned.concat();
    let kept = map.read(&contested, |_, value| value.slot()).expect("the key stays");
    all.push(kept);
    all.sort_unstable();
    let before = all.len();
    all.dedup();
    assert_eq!(all.len(), before, "a value was returned twice: {returned:?}, kept {kept}");
    assert_eq!(all.len(), inserted.load(Ordering::Relaxed), "a value was lost: {returned:?}, kept {kept}");
    map.check_invariants(len as usize + 1);
}

/// `get_or_insert` of a key while another thread removes it, over and
/// over, at a leaf that merges and splits on the way: what `get_or_insert`
/// runs its closure on is always a live value of that key, and the slot it
/// returns was the map's at that moment (the thread's own, or one the
/// other thread's removal returns later).
#[test]
fn get_or_insert_races_removals_of_the_same_key() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let (mut map, len) = shapes.one_removal_from_lowering(&fx, 1);
    let contested = key(len - 1);
    fx.race();
    let (seen, removed) = thread::scope(|scope| {
        let getter = scope.spawn(|| {
            let mut seen = Vec::new();
            for _ in 0..4 {
                let slot = map.get_or_insert(contested, fx.value(contested), |_, value| {
                    value.check(contested);
                    value.slot()
                });
                seen.push(slot);
            }
            seen
        });
        let remover = scope.spawn(|| {
            let mut removed = Vec::new();
            for _ in 0..4 {
                if let Some((_, value)) = map.remove_entry(&contested) {
                    value.check(contested);
                    removed.push(value.slot());
                }
            }
            removed
        });
        (getter.join().unwrap(), remover.join().unwrap())
    });
    let kept = map.read(&contested, |_, value| value.slot());
    for slot in &seen {
        assert!(
            removed.contains(slot) || kept == Some(*slot),
            "get_or_insert saw slot {slot}, which was neither removed later ({removed:?}) nor kept ({kept:?})"
        );
    }
    map.check_invariants(len as usize - 1 + usize::from(kept.is_some()));
}

/// A batched insert of eight keys spread over the tree, while a scan runs
/// and a removal merges the last leaf: the batch reports no old value, the
/// scan hands out every steady key once, in order, and all eight are there
/// after.
#[test]
fn batched_inserts_race_scans_and_a_merge() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let (mut map, len) = shapes.one_removal_from_lowering(&fx, 1);
    let steady: Vec<u64> = (0..len - 1).map(key).collect();
    let batch: [u64; 8] = core::array::from_fn(|i| key(i as u64 * len / 8) + 5);
    fx.race();
    thread::scope(|scope| {
        scope.spawn(|| {
            let entries = batch.map(|k| (k, fx.value(k)));
            let old = map.insert_many(entries);
            assert!(old.iter().all(Option::is_none), "a batched insert replaced a value");
        });
        scope.spawn(|| assert!(map.remove(&key(len - 1)).is_some()));
        scope.spawn(|| {
            assert_visited_in_order(&scanned_keys(&map, .., same, false), &steady);
            assert_visited_in_order(&scanned_keys(&map, .., same, true), &steady);
        });
    });
    for k in batch {
        assert!(map.read(&k, |_, value| value.check(k)).is_some(), "batched key {k} missing");
    }
    map.check_invariants(steady.len() + batch.len());
}

/// Scans that break at their first entry, and `is_empty`, which is one,
/// while the last key is removed and the tree refilled: a scan that
/// breaks lets go of its leaf, and sees an entry or none, never garbage.
#[test]
fn breaking_scans_and_is_empty_race_the_tree_emptying_and_refilling() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let mut map = map_of(&fx, 1);
    let refill: Vec<u64> = (0..shapes.capacity[0] + 1).map(|i| key(i + 1)).collect();
    fx.race();
    thread::scope(|scope| {
        scope.spawn(|| {
            assert!(map.remove(&key(0)).is_some());
            for &k in &refill {
                assert!(map.insert(k, fx.value(k)).is_none());
            }
        });
        scope.spawn(|| {
            for _ in 0..6 {
                let _ = map.is_empty();
                let first = map.scan::<u64, u64>(.., |k, value| {
                    value.check(*k);
                    ControlFlow::Break(*k)
                });
                let last = map.scan_rev::<u64, u64>(.., |k, value| {
                    value.check(*k);
                    ControlFlow::Break(*k)
                });
                if let (Some(first), Some(last)) = (first, last) {
                    assert!(first <= last || first == key(0), "first {first} after last {last}");
                }
            }
        });
    });
    assert!(!map.is_empty());
    assert_eq!(map.node_lengths().len(), 2, "the refill split the root leaf");
    map.check_invariants(refill.len());
}

/// User closures that panic inside `update`, `read` and `remove_if`, each
/// caught in its thread, while a fourth thread reads and inserts into the
/// same leaf: a panic lets go of the leaf's latch (a leaked latch would
/// leave the fourth thread spinning to the depth bound), and leaves the
/// entry as it was.
#[test]
fn panicking_closures_let_other_threads_through() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let (mut map, len) = shapes.full(&fx, 1);
    let (target, inserted) = (key(len / 2), key(len / 2) + 5);
    let target_slot = map.read(&target, |_, value| value.slot()).unwrap();
    fx.race();
    thread::scope(|scope| {
        let map = &map;
        scope.spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                map.update(&target, |_, _| panic!("expected panic in user code (update)"))
            }));
            assert!(r.is_err());
        });
        scope.spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                map.read(&target, |_, _| panic!("expected panic in user code (read)"))
            }));
            assert!(r.is_err());
        });
        scope.spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                map.remove_if(&target, |_, _| panic!("expected panic in user code (remove_if)"))
            }));
            assert!(r.is_err());
        });
        scope.spawn(|| {
            for _ in 0..3 {
                let slot = map.read(&target, |_, value| {
                    value.check(target);
                    value.slot()
                });
                assert_eq!(slot, Some(target_slot), "the entry changed under a panicking closure");
            }
            assert!(map.insert(inserted, fx.value(inserted)).is_none());
            assert!(map.read(&inserted, |_, value| value.check(inserted)).is_some());
        });
    });
    assert_eq!(map.read(&target, |_, value| value.slot()), Some(target_slot));
    map.check_invariants(len as usize + 1);
}

/// Six threads on the keys of one leaf and a half: the root leaf splits
/// and merges back under more contention than three threads make, and
/// every thread's answers match its model.
#[test]
fn six_threads_match_their_models_around_one_leaf() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let capacity = shapes.capacity[0];
    let map = map_of(&fx, capacity / 2);
    let owned = dealt((0..capacity + capacity / 2).map(key), 6);
    ModelRun {
        map,
        fx: &fx,
        key_of: |id| id,
        id_of: same,
        span: (Bound::Unbounded, Bound::Unbounded),
    }
    .run(owned, 8);
}

/// String keys whose internal-node windows tie: lookups of the keys around
/// a leaf's head, which the descent compares with latched (D49), while
/// another thread removes that head, its successor taking its place, and
/// inserts it back.
#[test]
fn tied_lookups_race_removing_and_restoring_a_head() {
    fn key_of(id: u64) -> String {
        format!("tie/{id:06}")
    }
    #[expect(clippy::ptr_arg, reason = "the map's keys are `String`s")]
    fn id_of(key: &String) -> u64 {
        key[key.len() - 6..].parse().expect("a numbered key")
    }
    let fx = Fixture::new();
    let mut map = fx.map::<String>();
    let mut len = 0;
    while map.node_lengths().len() < 2 {
        map.insert(key_of(len), fx.value(len));
        len += 1;
    }
    let leaves = map.leaf_keys();
    let head = leaves[1][0].clone();
    let head_id = id_of(&head);
    let around: Vec<String> = (head_id.saturating_sub(2)..head_id + 3).map(key_of).collect();
    fx.race();
    thread::scope(|scope| {
        scope.spawn(|| {
            for _ in 0..2 {
                let removed = map.remove_entry(&head).expect("the head is there");
                removed.1.check(head_id);
                drop(removed);
                assert!(map.insert(head.clone(), fx.value(head_id)).is_none());
            }
        });
        scope.spawn(|| {
            for _ in 0..3 {
                for k in &around {
                    let id = id_of(k);
                    let found = map.read(k, |key, value| {
                        assert_eq!(key, k);
                        value.check(id);
                    });
                    assert!(found.is_some() || id == head_id, "steady key {k} missing");
                    let many = map.read_many([k, &head], |key, value| value.check(id_of(key)));
                    assert!(many[0].is_some() || id == head_id);
                }
            }
        });
    });
    map.check_invariants(len as usize);
    assert!(map.read(&head, |_, value| value.check(head_id)).is_some());
}

// Scenarios from an audit of `tree/latched/` (tests/bbtree/README.md,
// "Audit"): where correctness rests on pages being type-stable and on the
// last-leaf pointer, with the watchpoint on the page about to be freed.

/// The last leaf freed by a merge that lowers the tree into the root leaf,
/// while appends wait for that very leaf (`append.rs` locks the leaf the
/// pointer names, then checks the pointer) and inserts split the root leaf
/// again, which takes the freed page back off the free list
/// (`pages.rs`): every key ends up in the tree, appends included.
#[test]
fn appends_race_the_last_leaf_freed_and_its_page_reused() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let (mut map, len) = shapes.one_removal_from_lowering(&fx, 1);
    let appended = [key(len), key(len + 1), key(len + 2)];
    let between: Vec<u64> = (0..shapes.capacity[0] + 2 - len)
        .map(|index| key(index) + 5)
        .collect();
    fx.race();
    thread::scope(|scope| {
        scope.spawn(|| assert!(map.remove(&key(len - 1)).is_some()));
        scope.spawn(|| {
            for k in appended {
                assert!(map.insert(k, fx.value(k)).is_none());
            }
        });
        scope.spawn(|| {
            for &k in &between {
                assert!(map.insert(k, fx.value(k)).is_none());
            }
        });
    });
    for k in appended.iter().chain(&between) {
        assert!(map.read(k, |_, value| value.check(*k)).is_some(), "key {k} missing");
    }
    map.check_invariants(len as usize - 1 + appended.len() + between.len());
}

/// Latched reads (`read` latches the leaf shared once the leaf's version
/// is still the descent's) of the keys of a leaf while a merge frees it
/// and a split takes its page back for other keys: a read that latched the
/// stale page lets go at once, the splitter's wait for it ends, and every
/// read finds its key's value or nothing.
#[test]
fn latched_reads_race_a_leaf_freed_and_reused_for_other_keys() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let (mut map, len) = shapes.one_removal_from_lowering(&fx, 1);
    let freed_leaf = map.leaf_keys()[1].clone();
    let first_leaf = map.leaf_keys()[0].clone();
    let between: Vec<u64> = (0..shapes.capacity[0] + 2 - len)
        .map(|index| key(index) + 5)
        .collect();
    fx.race();
    thread::scope(|scope| {
        scope.spawn(|| {
            assert!(map.remove(&key(len - 1)).is_some());
            for &k in &between {
                assert!(map.insert(k, fx.value(k)).is_none());
            }
        });
        scope.spawn(|| {
            for _ in 0..3 {
                for k in &freed_leaf {
                    let found = map.read(k, |_, value| value.check(*k));
                    assert!(found.is_some() || *k == key(len - 1), "key {k} missing");
                }
                for k in &first_leaf {
                    assert!(map.read(k, |_, value| value.check(*k)).is_some());
                }
                let found = map.read_many([&freed_leaf[0], &first_leaf[0]], |k, value| {
                    value.check(*k);
                    *k
                });
                assert_eq!(found, [Some(freed_leaf[0]), Some(first_leaf[0])]);
            }
        });
        scope.spawn(|| {
            let steady: Vec<u64> = (0..len - 1).map(key).collect();
            for _ in 0..2 {
                assert_visited_in_order(&scanned_keys(&map, .., same, false), &steady);
            }
        });
    });
    assert_eq!(map.node_lengths().len(), 2, "the root leaf split again");
    map.check_invariants(len as usize - 1 + between.len());
}

/// A removal that merges internal nodes and lowers the tree from three
/// levels to two, freeing an internal page, then appends that raise it
/// back, taking the internal page off the free list for the new level,
/// while threads look keys up through the changing levels and scan.
#[test]
fn internal_pages_freed_and_reused_under_lookups_and_scans() {
    let shapes = Shapes::probe();
    let fx = Fixture::new();
    let (mut map, len) = shapes.one_removal_from_lowering(&fx, 2);
    let raised = shapes.capacity[1] + 1;
    let steady: Vec<u64> = (0..len - 1).map(key).collect();
    let probes: Vec<u64> = (0..8).map(|i| key(i * (len - 2) / 7)).collect();
    fx.race();
    thread::scope(|scope| {
        scope.spawn(|| {
            assert!(map.remove(&key(len - 1)).is_some());
            for index in len - 1..=raised {
                assert!(map.insert(key(index), fx.value(key(index))).is_none());
            }
        });
        scope.spawn(|| {
            for _ in 0..3 {
                for &k in &probes {
                    assert!(map.read(&k, |_, value| value.check(k)).is_some(), "key {k} missing");
                }
                let found = map.read_many([&probes[0], &probes[7], &key(len - 1)], |k, value| {
                    value.check(*k);
                    *k
                });
                assert_eq!(found[0], Some(probes[0]));
                assert_eq!(found[1], Some(probes[7]));
            }
        });
        scope.spawn(|| {
            assert_visited_in_order(&scanned_keys(&map, .., same, false), &steady);
            assert_visited_in_order(&scanned_keys(&map, .., same, true), &steady);
        });
    });
    assert_eq!(map.node_lengths().len(), 3, "the tree was raised back");
    map.check_invariants(raised as usize + 1);
}

/// Latch-free copies while the tree lowers into its root leaf and splits
/// again, over and over, the freed page reused each time: a copy comes from
/// the leaf that holds the key at that version, never from the page's
/// stale or half-rewritten bytes.
#[test]
fn copied_lookups_race_pages_churning() {
    let shapes = CopiedShapes::probe();
    let pages = Pages::new();
    let (mut map, len) = shapes.one_removal_from_lowering(&pages, 1);
    let last = key(len - 1);
    let steady: Vec<u64> = (0..len - 1).map(key).collect();
    let between: Vec<u64> = (0..shapes.capacity[0] + 2 - len)
        .map(|index| key(index) + 5)
        .collect();
    Race::start(&pages);
    thread::scope(|scope| {
        scope.spawn(|| {
            // Lower, split again, lower again (the split halves merge back
            // once the between keys go), and split once more.
            for round in 0..2 {
                assert_eq!(map.remove(&last), Some(copied_value(last)));
                for &k in &between {
                    assert!(map.insert(k, copied_value(k)).is_none(), "round {round}");
                }
                for &k in &between {
                    assert_eq!(map.remove(&k), Some(copied_value(k)));
                }
                assert!(map.insert(last, copied_value(last)).is_none());
            }
        });
        scope.spawn(|| {
            for _ in 0..6 {
                for &k in &steady {
                    assert_eq!(map.get_copied(&k), Some(copied_value(k)), "steady key {k}");
                }
                for &k in &between {
                    assert_copied(k, map.get_copied(&k), false);
                    let _ = map.contains_key(&k);
                }
                assert_copied(last, map.get_copied(&last), false);
                let found = map.get_many_copied([&steady[0], &between[0], &last]);
                assert_eq!(found[0], Some(copied_value(steady[0])));
                assert_copied(between[0], found[1], false);
                assert_copied(last, found[2], false);
            }
        });
    });
    for &k in &steady {
        assert_eq!(map.get_copied(&k), Some(copied_value(k)));
    }
    assert_eq!(map.get_copied(&last), Some(copied_value(last)));
    map.check_invariants(len as usize);
}
