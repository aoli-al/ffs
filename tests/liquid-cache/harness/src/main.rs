//! Concurrency harness for liquid-cache's `LiquidCache`, run under `ffs explore`.
//!
//! The expensive part (tokio, the t4 store, a pre-filled cache) happens once,
//! then `ffs_checkpoint` marks the exploration root. Every branch runs the same
//! scripted operations (derived from `--seed`) from several threads and checks:
//!
//! * every `get` returns the bytes inserted for that (key, identity) or nothing;
//! * no panic / abort anywhere (the index, budget and policy code `expect`s a lot);
//! * once every worker has joined, the cache's own stats are consistent: the
//!   memory budget equals the bytes of the in-memory entries, the entry count
//!   equals what `for_each_entry` visits, and (with `--check-disk`) the disk
//!   budget equals the bytes of the on-disk entries.
//!
//! Natively this is just a stress test; under ffs the interleavings are chosen
//! by the scheduler and a failing one is saved and replayable.

use std::sync::Arc;
use std::thread;

use arrow::array::{Array, ArrayRef, AsArray, Int64Array};
use arrow::datatypes::Int64Type;
use liquid_cache::cache::{
    AlwaysHydrate, CacheEntry, EntryID, Evict, LiquidCache, LiquidCacheBuilder, LiquidPolicy,
    PrefetchResult, TranscodeEvict,
};

mod ffs {
    use std::ffi::CString;
    const BASE: i64 = 0x0ff5000;

    unsafe fn raw(nr: i64, a: i64, b: i64) -> i64 {
        let ret: i64;
        unsafe {
            core::arch::asm!("syscall", inlateout("rax") nr => ret, in("rdi") a, in("rsi") b,
                lateout("rcx") _, lateout("r11") _, options(nostack));
        }
        ret
    }
    pub fn checkpoint(name: &str) {
        let c = CString::new(name).unwrap();
        unsafe { raw(BASE + 1, c.as_ptr() as i64, 0) };
    }
    pub fn assert(cond: bool, msg: &str) {
        if !cond {
            eprintln!("[harness] ASSERTION FAILED: {msg}");
        }
        let c = CString::new(msg).unwrap();
        let r = unsafe { raw(BASE + 2, cond as i64, c.as_ptr() as i64) };
        if !cond && r != 0 {
            // Not under ffs: fail the way a test would.
            std::process::exit(3);
        }
    }
    pub fn end() {
        unsafe { raw(BASE + 7, 0, 0) };
    }
    pub fn watch(addr: usize, len: usize) {
        unsafe { raw(BASE + 8, addr as i64, len as i64) };
    }
}

#[derive(Clone, Debug)]
struct Config {
    threads: usize,
    ops: usize,
    keys: usize,
    identities: u64,
    rows: usize,
    mem_entries: f64,
    disk_bytes: usize,
    seed: u64,
    eviction: String,
    prefill: usize,
    prefill_flush: bool,
    concurrent_evict: bool,
    check_disk: bool,
    /// Register `budget.used_memory_bytes` as an ffs watchpoint (`ffs_watch`).
    watch_budget: bool,
    flush_pct: u32,
    stats_pct: u32,
    verbose: bool,
    /// Directory for the t4 store; created here, must not exist (a replay
    /// must start from the same empty state as the recording).
    store_dir: Option<String>,
    /// Explicit per-thread scripts (`--script "get:0:0,insert:1:0,flush,stats"`), one per thread.
    scripts: Vec<String>,
}

impl Config {
    fn parse() -> Self {
        let mut c = Config {
            threads: 3,
            ops: 20,
            keys: 6,
            identities: 2,
            rows: 128,
            mem_entries: 3.5,
            disk_bytes: 1 << 20,
            seed: 1,
            eviction: "transcode".into(),
            prefill: 4,
            prefill_flush: true,
            concurrent_evict: true,
            check_disk: false,
            watch_budget: false,
            flush_pct: 8,
            stats_pct: 10,
            verbose: false,
            scripts: Vec::new(),
            store_dir: None,
        };
        let args: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < args.len() {
            let a = args[i].as_str();
            let mut val = || {
                i += 1;
                args.get(i).cloned().unwrap_or_else(|| panic!("{a} needs a value"))
            };
            match a {
                "--threads" => c.threads = val().parse().unwrap(),
                "--ops" => c.ops = val().parse().unwrap(),
                "--keys" => c.keys = val().parse().unwrap(),
                "--identities" => c.identities = val().parse().unwrap(),
                "--rows" => c.rows = val().parse().unwrap(),
                "--mem-entries" => c.mem_entries = val().parse().unwrap(),
                "--disk-bytes" => c.disk_bytes = val().parse().unwrap(),
                "--seed" => c.seed = val().parse().unwrap(),
                "--eviction" => c.eviction = val(),
                "--prefill" => c.prefill = val().parse().unwrap(),
                "--flush-pct" => c.flush_pct = val().parse().unwrap(),
                "--stats-pct" => c.stats_pct = val().parse().unwrap(),
                "--no-prefill-flush" => c.prefill_flush = false,
                "--sequential-evict" => c.concurrent_evict = false,
                "--check-disk" => c.check_disk = true,
                "--watch-budget" => c.watch_budget = true,
                "--script" => c.scripts.push(val()),
                "--store-dir" => c.store_dir = Some(val()),
                "-v" => c.verbose = true,
                _ => panic!("unknown argument {a}"),
            }
            i += 1;
        }
        if !c.scripts.is_empty() {
            c.threads = c.scripts.len();
        }
        c
    }
}

fn parse_script(text: &str) -> Vec<Op> {
    text.split(',')
        .map(|item| item.trim())
        .filter(|item| !item.is_empty())
        .map(|item| {
            let parts: Vec<&str> = item.split(':').collect();
            let key = || parts.get(1).expect("key").parse::<usize>().unwrap();
            let ident = || parts.get(2).map(|v| v.parse::<u64>().unwrap()).unwrap_or(0);
            match parts[0] {
                "insert" => Op::Insert(key(), ident()),
                "get" => Op::Get(key(), ident()),
                "prefetch" => Op::Prefetch(key(), ident()),
                "liquid" => Op::TryReadLiquid(key(), ident()),
                "contains" => Op::Contains(key(), ident()),
                "stats" => Op::Stats,
                "flush" => Op::Flush,
                other => panic!("unknown op {other}"),
            }
        })
        .collect()
}

/// Deterministic content for (key, identity): a get must return exactly this or nothing.
fn expected(key: usize, identity: u64, rows: usize) -> ArrayRef {
    let base = key as i64 * 1_000_003 + identity as i64 * 7_919;
    Arc::new(Int64Array::from_iter_values(base..base + rows as i64))
}

fn check_content(what: &str, key: usize, identity: u64, rows: usize, got: &ArrayRef) {
    let want = expected(key, identity, rows);
    let ok = got.data_type() == want.data_type()
        && got.len() == want.len()
        && got.as_primitive::<Int64Type>().values() == want.as_primitive::<Int64Type>().values();
    ffs::assert(
        ok,
        &format!("{what}: wrong data for key {key} identity {identity}: got {got:?}"),
    );
}

#[derive(Clone, Copy, Debug)]
enum Op {
    Insert(usize, u64),
    Get(usize, u64),
    Prefetch(usize, u64),
    TryReadLiquid(usize, u64),
    Contains(usize, u64),
    Stats,
    Flush,
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn script(c: &Config, thread: usize) -> Vec<Op> {
    if let Some(text) = c.scripts.get(thread) {
        return parse_script(text);
    }
    let mut rng = Lcg(c.seed.wrapping_mul(0x9E3779B97F4A7C15) ^ (thread as u64 + 1) * 0x1234567);
    (0..c.ops)
        .map(|_| {
            let k = rng.below(c.keys as u64) as usize;
            let id = rng.below(c.identities);
            let r = rng.below(100) as u32;
            if r < c.flush_pct {
                Op::Flush
            } else if r < c.flush_pct + c.stats_pct {
                Op::Stats
            } else {
                match rng.below(10) {
                    0..=3 => Op::Insert(k, id),
                    4..=7 => Op::Get(k, id),
                    8 => {
                        if rng.below(2) == 0 {
                            Op::Prefetch(k, id)
                        } else {
                            Op::TryReadLiquid(k, id)
                        }
                    }
                    _ => Op::Contains(k, id),
                }
            }
        })
        .collect()
}

async fn run_op(cache: &Arc<LiquidCache>, c: &Config, thread: usize, op: Op) {
    if c.verbose {
        eprintln!("[harness] T{thread} {op:?}");
    }
    match op {
        Op::Insert(k, id) => {
            let r = cache.insert(EntryID::from(k), id, expected(k, id, c.rows)).await;
            if c.verbose && r.is_err() {
                eprintln!("[harness] T{thread} insert {k}/{id}: cache full");
            }
        }
        Op::Get(k, id) => {
            if let Some(got) = cache.get(&EntryID::from(k), id).await {
                check_content("get", k, id, c.rows, &got);
            }
        }
        Op::Prefetch(k, id) => {
            if let PrefetchResult::Snapshot(entry) = cache.prefetch(&EntryID::from(k), id).await {
                ffs::assert(
                    matches!(&*entry, CacheEntry::MemoryArrow(_) | CacheEntry::MemoryLiquid(_)),
                    "prefetch returned a disk entry as a snapshot",
                );
                if let Some(got) = cache.read_entry(&EntryID::from(k), id, &entry, None, None).await {
                    check_content("read_entry", k, id, c.rows, &got);
                }
            }
        }
        Op::TryReadLiquid(k, id) => {
            if let Some(liquid) = cache.try_read_liquid(&EntryID::from(k), id).await {
                check_content("try_read_liquid", k, id, c.rows, &liquid.to_arrow_array());
            }
        }
        Op::Contains(k, id) => {
            let _ = cache.contains(&EntryID::from(k), id);
        }
        Op::Stats => {
            let _ = cache.stats();
        }
        Op::Flush => {
            let _ = cache.flush_all_to_disk().await;
        }
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn final_checks(cache: &Arc<LiquidCache>, c: &Config) {
    let s = cache.stats();
    let mut visited = 0usize;
    let mut mem_bytes = 0usize;
    let mut disk_bytes = 0usize;
    cache.for_each_entry(|_, entry| {
        visited += 1;
        match entry {
            CacheEntry::MemoryArrow(_) | CacheEntry::MemoryLiquid(_) => {
                mem_bytes += entry.memory_usage_bytes()
            }
            CacheEntry::DiskArrow { disk_bytes: b, .. } | CacheEntry::DiskLiquid { disk_bytes: b, .. } => {
                disk_bytes += b + 8
            }
        }
    });
    eprintln!(
        "[harness] final: entries={} visited={} mem_budget={} mem_entries={} disk_budget={} disk_entries={} max_mem={}",
        s.total_entries, visited, s.memory_usage_bytes, mem_bytes, s.disk_usage_bytes, disk_bytes, s.max_memory_bytes
    );
    eprintln!("[harness] runtime stats: {:?}", s.runtime);
    ffs::assert(
        s.total_entries == visited,
        &format!("entry count {} but for_each_entry visited {visited}", s.total_entries),
    );
    ffs::assert(
        s.memory_usage_bytes == mem_bytes,
        &format!(
            "memory budget drift: budget says {} bytes used, entries hold {mem_bytes}",
            s.memory_usage_bytes
        ),
    );
    ffs::assert(
        s.memory_usage_bytes <= s.max_memory_bytes,
        &format!("memory budget exceeded: {} > {}", s.memory_usage_bytes, s.max_memory_bytes),
    );
    if c.check_disk {
        ffs::assert(
            s.disk_usage_bytes == disk_bytes,
            &format!(
                "disk budget drift: budget says {} bytes used, entries hold {disk_bytes}",
                s.disk_usage_bytes
            ),
        );
    }
    // Every entry still readable and holding what was inserted for it.
    let rt = runtime();
    rt.block_on(async {
        for k in 0..c.keys {
            for id in 0..c.identities {
                if let Some(got) = cache.get(&EntryID::from(k), id).await {
                    check_content("final get", k, id, c.rows, &got);
                }
            }
        }
    });
}

fn main() {
    let c = Config::parse();
    // Kept, not deleted at exit: every ffs branch restores the store file
    // from its snapshot, and removing the directory at exit would defeat that.
    // Under ffs tempdir names are deterministic (virtual randomness), so a
    // second run collides with the first one's kept directory and takes a
    // different path through mkdir (EEXIST, retry): `--store-dir` names the
    // directory explicitly instead, and it must not exist yet.
    let tmp = match &c.store_dir {
        Some(dir) => {
            let dir = std::path::PathBuf::from(dir);
            std::fs::create_dir(&dir).unwrap_or_else(|e| panic!("create {}: {e}", dir.display()));
            dir
        }
        None => tempfile::tempdir().unwrap().keep(),
    };
    let rt = runtime();
    let entry_bytes = expected(0, 0, c.rows).get_array_memory_size();
    let max_memory = (entry_bytes as f64 * c.mem_entries) as usize;
    let cache = rt.block_on(async {
        let store = t4::mount(tmp.join("cache.t4")).await.expect("mount");
        let builder = LiquidCacheBuilder::new()
            .with_max_memory_bytes(max_memory)
            .with_max_disk_bytes(c.disk_bytes)
            .with_cache_policy(Box::new(LiquidPolicy::new()))
            .with_hydration_policy(Box::new(AlwaysHydrate::new()))
            .with_store(store)
            .with_evict_victims_concurrently(c.concurrent_evict);
        let builder = match c.eviction.as_str() {
            "transcode" => builder.with_eviction_policy(Box::new(TranscodeEvict)),
            "evict" => builder.with_eviction_policy(Box::new(Evict)),
            other => panic!("unknown eviction policy {other}"),
        };
        let cache = builder.build().await;
        for k in 0..c.prefill.min(c.keys) {
            cache.insert(EntryID::from(k), 0, expected(k, 0, c.rows)).await.unwrap();
        }
        if c.prefill_flush {
            cache.flush_all_to_disk().await.unwrap();
        }
        cache
    });
    let s = cache.stats();
    eprintln!(
        "[harness] {c:?}\n[harness] entry={entry_bytes} B, max_memory={max_memory} B; prefilled {} entries ({} mem / {} disk bytes)",
        s.total_entries, s.memory_usage_bytes, s.disk_usage_bytes
    );

    // Heap addresses are deterministic under ffs (no ASLR, virtual randomness),
    // so these can be fed back as `ffs explore --watch 0x...` hardware
    // watchpoints: the budget counters are touched inside the index
    // read-modify-write windows that the races live in.  Offsets 24 and 32
    // are where rustc put `used_memory_bytes` and `used_disk_bytes` (the
    // words dump below verifies: observer, max_memory, max_disk, used_memory,
    // used_disk).
    let budget_addr = cache.budget() as *const _ as usize;
    if c.watch_budget {
        ffs::watch(budget_addr + 24, 8);
    }
    // Field layout check: the words of BudgetAccounting (max_memory,
    // max_disk, used_memory, used_disk, observer pointer in some order).
    let words: Vec<u64> = (0..5).map(|i| unsafe { *((budget_addr + 8 * i) as *const u64) }).collect();
    eprintln!("[harness] budget struct words at 0x{budget_addr:x}: {words:x?}");
    // On a panic anywhere, print the cache's internal event trace: it is the
    // interleaving of cache operations that led to the failure.
    let hook_cache = cache.clone();
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        if std::thread::current().name() != Some("main") {
            eprintln!("[harness] event trace at panic:\n{:#?}", hook_cache.consume_event_trace());
        }
    }));
    eprintln!(
        "[harness] watch candidates: budget.used_memory_bytes=0x{:x} budget.used_disk_bytes=0x{:x}",
        budget_addr + 24,
        budget_addr + 32
    );

    ffs::checkpoint("ready");

    let handles: Vec<_> = (0..c.threads)
        .map(|t| {
            let cache = cache.clone();
            let c = c.clone();
            let ops = script(&c, t);
            thread::Builder::new()
                .name(format!("worker-{t}"))
                .spawn(move || {
                    let rt = runtime();
                    rt.block_on(async {
                        for op in ops {
                            run_op(&cache, &c, t, op).await;
                        }
                    });
                })
                .unwrap()
        })
        .collect();
    for h in handles {
        h.join().expect("worker panicked");
    }
    ffs::end();
    final_checks(&cache, &c);
    if c.verbose {
        eprintln!("[harness] event trace: {:#?}", cache.consume_event_trace());
    }
    eprintln!("[harness] ok");
    drop(rt);
}
