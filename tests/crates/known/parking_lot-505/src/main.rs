//! parking_lot issue #505 (closed by PR #533): a `read_recursive()` that
//! parks after a parked writer is not woken when the current writer
//! unlocks (`wake_parked_threads` stops at the first writer it wakes).
//! Four threads make that order reachable:
//!
//!   W0 holds the write lock while A, W1 and B arrive;
//!   A: read()          -> parks behind W0, later waits on a condvar for B
//!                         while holding its read lock;
//!   W1: write()        -> parks;
//!   B: read_recursive() -> parks (W0 holds the lock) after W1 (deadlock)
//!                          or before it (fine);
//!   W0 unlocks: wakes A (reader), then W1 (writer), then stops.
//!
//! A holds read and waits for B; W1 waits for A; B is parked for good.
//! PR #533 removed `read_recursive` and added `RecursiveRwLock`
//! (reader-preferring); `--features post` builds that version.
mod ffs;
use parking_lot::{Condvar, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[cfg(not(feature = "post"))]
type Lock = parking_lot::RwLock<u32>;
#[cfg(feature = "post")]
type Lock = parking_lot::RecursiveRwLock<u32>;

struct Shared {
    lock: Lock,
    arrived: AtomicUsize,
    filled: Mutex<bool>,
    cv: Condvar,
}

fn main() {
    let s = Arc::new(Shared { lock: Lock::new(0), arrived: AtomicUsize::new(0), filled: Mutex::new(false), cv: Condvar::new() });
    let w0 = {
        let s = s.clone();
        std::thread::spawn(move || {
            let g = s.lock.write();
            s.arrived.fetch_add(1, Ordering::SeqCst);
            while s.arrived.load(Ordering::SeqCst) < 4 {
                std::thread::yield_now();
            }
            // Give the others a chance to park; the scheduler decides.
            std::thread::sleep(std::time::Duration::from_millis(1));
            drop(g);
        })
    };
    let wait_for_w0 = |s: &Shared| {
        while s.arrived.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }
    };
    let a = {
        let s = s.clone();
        std::thread::spawn(move || {
            wait_for_w0(&s);
            s.arrived.fetch_add(1, Ordering::SeqCst);
            let g = s.lock.read();
            let mut f = s.filled.lock();
            while !*f {
                s.cv.wait(&mut f);
            }
            drop(f);
            drop(g);
        })
    };
    let w1 = {
        let s = s.clone();
        std::thread::spawn(move || {
            wait_for_w0(&s);
            s.arrived.fetch_add(1, Ordering::SeqCst);
            let mut g = s.lock.write();
            *g += 1;
        })
    };
    let b = {
        let s = s.clone();
        std::thread::spawn(move || {
            wait_for_w0(&s);
            s.arrived.fetch_add(1, Ordering::SeqCst);
            #[cfg(not(feature = "post"))]
            let g = s.lock.read_recursive();
            #[cfg(feature = "post")]
            let g = s.lock.read();
            *s.filled.lock() = true;
            s.cv.notify_all();
            drop(g);
        })
    };
    for t in [w0, a, w1, b] {
        t.join().unwrap();
    }
    println!("ok");
}
