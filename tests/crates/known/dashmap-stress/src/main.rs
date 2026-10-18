//! dashmap under concurrent insert/entry/remove/retain/iter/shrink:
//! every increment is accounted for (map values + removed values ==
//! increments), len() agrees with iteration, and nothing deadlocks or
//! panics.  Keys share shards (4 shards, 8 keys) so that cross-shard and
//! same-shard interleavings both occur.
mod ffs;
use dashmap::DashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

fn main() {
    let threads: usize = std::env::args().nth(1).map(|s| s.parse().unwrap()).unwrap_or(3);
    let ops: usize = std::env::args().nth(2).map(|s| s.parse().unwrap()).unwrap_or(12);
    let map: Arc<DashMap<u32, u64>> = Arc::new(DashMap::with_capacity_and_shard_amount(8, 4));
    for k in 0..4u32 {
        map.insert(k, 1);
    }
    let incs = Arc::new(AtomicU64::new(4));
    let removed = Arc::new(AtomicU64::new(0));
    ffs::checkpoint("ready");
    let mut hs = Vec::new();
    for t in 0..threads {
        let (map, incs, removed) = (map.clone(), incs.clone(), removed.clone());
        hs.push(std::thread::spawn(move || {
            let mut x = 0x9e3779b97f4a7c15u64 ^ (t as u64 + 1) * 0x100000001b3;
            for _ in 0..ops {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let k = (x % 8) as u32;
                match (x >> 8) % 7 {
                    0 | 1 => {
                        *map.entry(k).or_insert(0) += 1;
                        incs.fetch_add(1, Ordering::SeqCst);
                    }
                    2 => {
                        if let Some((_, v)) = map.remove(&k) {
                            removed.fetch_add(v, Ordering::SeqCst);
                        }
                    }
                    3 => {
                        let n = map.iter().count();
                        assert!(n <= 8, "iter saw {n} entries");
                    }
                    4 => {
                        if let Some(mut r) = map.get_mut(&k) {
                            *r += 1;
                            incs.fetch_add(1, Ordering::SeqCst);
                        }
                    }
                    5 => {
                        let mut gone = 0;
                        map.retain(|key, v| {
                            if key % 3 == 2 {
                                gone += *v;
                                false
                            } else {
                                true
                            }
                        });
                        removed.fetch_add(gone, Ordering::SeqCst);
                    }
                    _ => {
                        let _ = map.len();
                        map.shrink_to_fit();
                    }
                }
            }
        }));
    }
    for h in hs {
        h.join().unwrap();
    }
    let sum: u64 = map.iter().map(|r| *r.value()).sum();
    let total = sum + removed.load(Ordering::SeqCst);
    ffs::assert(total == incs.load(Ordering::SeqCst), &format!("lost or duplicated increments: {} in map + {} removed != {} increments", sum, removed.load(Ordering::SeqCst), incs.load(Ordering::SeqCst)));
    ffs::assert(map.len() == map.iter().count(), "len() disagrees with iteration");
    println!("ok");
}
