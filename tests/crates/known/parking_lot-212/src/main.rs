//! parking_lot issue #212: a thread that holds a read lock and takes a
//! second `read()` deadlocks against a thread waiting in `write()` (the
//! second read waits for the writer, the writer waits for the first read).
//! The report loops for a second to hit it; here each thread does a bounded
//! number of rounds and ffs's deadlock detector decides.
use parking_lot::RwLock;
use std::sync::Arc;

fn main() {
    let rounds: usize = std::env::args().nth(1).map(|s| s.parse().unwrap()).unwrap_or(20);
    let lock = Arc::new(RwLock::new(()));
    let l = lock.clone();
    let t0 = std::thread::spawn(move || {
        for _ in 0..rounds {
            let _a = l.read();
            let _b = l.read();
        }
    });
    let l = lock.clone();
    let t1 = std::thread::spawn(move || {
        for _ in 0..rounds {
            let _a = l.write();
        }
    });
    t0.join().unwrap();
    t1.join().unwrap();
    println!("ok");
}
