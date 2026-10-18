//! crossbeam-channel: producers send tagged messages over a zero-capacity,
//! a bounded(1) and an unbounded channel; consumers `select!` over all
//! three with timeouts and `try_recv`, while one producer disconnects
//! early.  Every message must be received exactly once, nothing may
//! deadlock, and `recv_timeout` on a disconnected channel must report it.
mod ffs;
use crossbeam_channel::{bounded, select, unbounded, RecvTimeoutError, Sender};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

const PER: usize = 6;

fn main() {
    let (z_tx, z_rx) = bounded::<usize>(0);
    let (b_tx, b_rx) = bounded::<usize>(1);
    let (u_tx, u_rx) = unbounded::<usize>();
    let seen: Arc<Vec<AtomicU8>> = Arc::new((0..3 * PER).map(|_| AtomicU8::new(0)).collect());
    ffs::checkpoint("ready");
    let mut producers = Vec::new();
    for (i, tx) in [z_tx, b_tx, u_tx].into_iter().enumerate() {
        producers.push(std::thread::spawn(move || {
            let tx: Sender<usize> = tx;
            for j in 0..PER {
                let m = i * PER + j;
                if i == 1 && j % 2 == 1 {
                    // Exercise the timeout path of the bounded channel.
                    let mut msg = m;
                    while let Err(e) = tx.send_timeout(msg, Duration::from_millis(1)) {
                        if e.is_disconnected() {
                            return;
                        }
                        msg = e.into_inner();
                    }
                } else {
                    tx.send(m).unwrap();
                }
            }
            // The zero-capacity producer disconnects first; consumers must
            // keep draining the others.
        }));
    }
    let mut consumers = Vec::new();
    for c in 0..2 {
        let (z_rx, b_rx, u_rx, seen) = (z_rx.clone(), b_rx.clone(), u_rx.clone(), seen.clone());
        consumers.push(std::thread::spawn(move || {
            let mut open = [true; 3];
            let mut got = 0;
            while open.iter().any(|o| *o) {
                let r = if c == 0 {
                    select! {
                        recv(z_rx) -> m => (0, m),
                        recv(b_rx) -> m => (1, m),
                        recv(u_rx) -> m => (2, m),
                        default(Duration::from_millis(2)) => (3, Err(crossbeam_channel::RecvError)),
                    }
                } else {
                    // The other consumer polls with timeouts, in turn.
                    let which = got % 3;
                    let rx = [&z_rx, &b_rx, &u_rx][which];
                    match rx.recv_timeout(Duration::from_millis(1)) {
                        Ok(m) => (which, Ok(m)),
                        Err(RecvTimeoutError::Disconnected) => {
                            got += 1; // rotate past the closed channel
                            (which, Err(crossbeam_channel::RecvError))
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            got += 1; // rotate
                            (3, Err(crossbeam_channel::RecvError))
                        }
                    }
                };
                match r {
                    (3, _) => {
                        // Timed out: re-check which channels are still open.
                        for (i, rx) in [&z_rx, &b_rx, &u_rx].into_iter().enumerate() {
                            if open[i] {
                                match rx.try_recv() {
                                    Ok(m) => {
                                        let prev = seen[m].fetch_add(1, Ordering::SeqCst);
                                        ffs::assert(prev == 0, &format!("message {m} received twice"));
                                    }
                                    Err(e) if e.is_disconnected() => open[i] = false,
                                    Err(_) => {}
                                }
                            }
                        }
                    }
                    (i, Ok(m)) => {
                        got += 1;
                        ffs::assert(m / PER == i, &format!("message {m} came from channel {i}"));
                        let prev = seen[m].fetch_add(1, Ordering::SeqCst);
                        ffs::assert(prev == 0, &format!("message {m} received twice"));
                    }
                    (i, Err(_)) => open[i] = false,
                }
            }
        }));
    }
    for p in producers {
        p.join().unwrap();
    }
    for c in consumers {
        c.join().unwrap();
    }
    let missing: Vec<usize> = (0..3 * PER).filter(|m| seen[*m].load(Ordering::SeqCst) != 1).collect();
    ffs::assert(missing.is_empty(), &format!("messages not received exactly once: {missing:?}"));
    println!("ok");
}
