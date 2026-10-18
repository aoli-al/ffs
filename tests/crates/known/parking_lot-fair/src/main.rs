//! parking_lot PR #533, "Fix fair upgradable rwlock unlocking":
//! `unlock_upgradable_fair` called `unlock_upgradable_slow(false)`, so a
//! fair unlock of an upgradable guard did not hand the lock to the parked
//! upgradable reader; a barging thread could take it again.  Thread A
//! holds `upgradable_read`, B parks in `upgradable_read`, A unlocks fairly
//! and immediately re-acquires: with a fair handoff B must get the lock
//! first.  The first unlock of a process is always "eventually fair"
//! (the bucket's fairness timer starts expired), so the property is
//! checked from the second round on.
mod ffs;
use parking_lot::lock_api::RwLockUpgradableReadGuard;
use parking_lot::RwLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// One round; true if A re-acquired before the parked B got the lock.
fn round(lock: &Arc<RwLock<u32>>) -> bool {
    let order = Arc::new(AtomicUsize::new(0));
    let b_arrived = Arc::new(AtomicUsize::new(0));
    let g = lock.upgradable_read();
    let b = {
        let (lock, order, b_arrived) = (lock.clone(), order.clone(), b_arrived.clone());
        std::thread::spawn(move || {
            b_arrived.store(1, Ordering::SeqCst);
            let _g = lock.upgradable_read(); // parks: A holds the upgradable slot
            order.compare_exchange(0, 2, Ordering::SeqCst, Ordering::SeqCst).ok();
        })
    };
    while b_arrived.load(Ordering::SeqCst) == 0 {
        std::thread::yield_now();
    }
    // parking_lot spins (with yields) about ten times before parking; no
    // sleep here, which would expire the fairness timer.
    for _ in 0..64 {
        std::thread::yield_now();
    }
    RwLockUpgradableReadGuard::unlock_fair(g);
    let g2 = lock.upgradable_read(); // barging re-acquire
    let first = order.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst);
    drop(g2);
    b.join().unwrap();
    first.is_ok()
}

fn main() {
    let lock = Arc::new(RwLock::new(0u32));
    let _warmup = round(&lock);
    for r in 2..=3 {
        let a_first = round(&lock);
        ffs::assert(!a_first, &format!("round {r}: fair unlock did not hand the upgradable lock to the parked thread; A re-acquired first"));
    }
    println!("ok");
}
