//! tokio sync primitives on a 2-worker multi-thread runtime, with the
//! properties their unit tests check one at a time, here all at once and
//! under ffs's schedules: Notify permits are neither lost nor duplicated
//! (notify_one / notified / enable / notify_waiters), a Semaphore never
//! exceeds its permits, every broadcast receiver sees every value or a
//! Lagged error with the right count, a bounded mpsc delivers in order per
//! sender, and a watch's last value is the final one.  A lost wakeup shows
//! up as a deadlock (ffs reports it), a counting error as an assertion.
mod ffs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, watch, Notify, Semaphore};

const ROUNDS: usize = 8;

fn main() {
    let scenario = std::env::args().nth(1).unwrap_or_else(|| "all".to_string());
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_time().build().unwrap();
    ffs::checkpoint("ready");
    rt.block_on(async {
        let mut tasks = Vec::new();
        if scenario == "all" || scenario == "notify" {
            tasks.push(tokio::spawn(notify_pingpong()));
            tasks.push(tokio::spawn(notify_waiters()));
        }
        if scenario == "all" || scenario == "semaphore" {
            tasks.push(tokio::spawn(semaphore()));
        }
        if scenario == "all" || scenario == "broadcast" {
            tasks.push(tokio::spawn(broadcast_lag()));
        }
        if scenario == "all" || scenario == "mpsc" {
            tasks.push(tokio::spawn(mpsc_order()));
        }
        if scenario == "all" || scenario == "watch" {
            tasks.push(tokio::spawn(watch_last()));
        }
        for t in tasks {
            t.await.unwrap();
        }
    });
    drop(rt);
    println!("ok");
}

async fn notify_pingpong() {
    // Two tasks alternate: each notify_one hands exactly one permit over.
    let a = Arc::new(Notify::new());
    let b = Arc::new(Notify::new());
    let count = Arc::new(AtomicUsize::new(0));
    let (a2, b2, c2) = (a.clone(), b.clone(), count.clone());
    let t = tokio::spawn(async move {
        for _ in 0..ROUNDS {
            a2.notified().await;
            c2.fetch_add(1, Ordering::SeqCst);
            b2.notify_one();
        }
    });
    for _ in 0..ROUNDS {
        a.notify_one();
        b.notified().await;
    }
    t.await.unwrap();
    ffs::assert(count.load(Ordering::SeqCst) == ROUNDS, "notify ping-pong lost a round");
    // A permit stored while nobody waits is consumed by the next notified().
    a.notify_one();
    a.notify_one(); // coalesces: still one permit
    a.notified().await;
    let pending = {
        let n = a.notified();
        tokio::pin!(n);
        n.as_mut().enable()
    };
    ffs::assert(!pending, "a second permit was stored after notify_one twice");
}

async fn notify_waiters() {
    // notify_waiters wakes exactly the futures that were enabled before it.
    let n = Arc::new(Notify::new());
    let woken = Arc::new(AtomicUsize::new(0));
    let mut hs = Vec::new();
    let armed = Arc::new(AtomicUsize::new(0));
    for _ in 0..3 {
        let (n, woken, armed) = (n.clone(), woken.clone(), armed.clone());
        hs.push(tokio::spawn(async move {
            let notified = n.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            armed.fetch_add(1, Ordering::SeqCst);
            notified.await;
            woken.fetch_add(1, Ordering::SeqCst);
        }));
    }
    while armed.load(Ordering::SeqCst) < 3 {
        tokio::task::yield_now().await;
    }
    n.notify_waiters();
    for h in hs {
        h.await.unwrap();
    }
    ffs::assert(woken.load(Ordering::SeqCst) == 3, "notify_waiters missed an enabled waiter");
    // A notify_one after notify_waiters must not be stolen by a stale future.
    let stale = n.notified();
    tokio::pin!(stale);
    n.notify_waiters();
    n.notify_one();
    stale.as_mut().await;
    let fresh = n.notified();
    tokio::pin!(fresh);
    ffs::assert(fresh.as_mut().enable(), "the permit of notify_one was consumed by a pre-notify_waiters future (#7965)");
}

async fn semaphore() {
    let sem = Arc::new(Semaphore::new(2));
    let inside = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let mut hs = Vec::new();
    for i in 0..5 {
        let (sem, inside, peak) = (sem.clone(), inside.clone(), peak.clone());
        hs.push(tokio::spawn(async move {
            for _ in 0..ROUNDS / 2 {
                let n = if i % 2 == 0 { 1 } else { 2 };
                let p = sem.acquire_many(n).await.unwrap();
                let now = inside.fetch_add(n as usize, Ordering::SeqCst) + n as usize;
                peak.fetch_max(now, Ordering::SeqCst);
                tokio::task::yield_now().await;
                inside.fetch_sub(n as usize, Ordering::SeqCst);
                drop(p);
            }
        }));
    }
    for h in hs {
        h.await.unwrap();
    }
    ffs::assert(peak.load(Ordering::SeqCst) <= 2, "semaphore let more than 2 permits out");
    ffs::assert(sem.available_permits() == 2, "permits leaked");
}

async fn broadcast_lag() {
    let (tx, _rx0) = broadcast::channel::<usize>(2);
    let mut hs = Vec::new();
    for r in 0..3 {
        let mut rx = tx.subscribe();
        hs.push(tokio::spawn(async move {
            let mut next = 0usize;
            loop {
                match rx.recv().await {
                    Ok(v) => {
                        ffs::assert(v == next, &format!("receiver {r}: got {v}, expected {next}"));
                        next = v + 1;
                        if r == 0 {
                            tokio::task::yield_now().await; // a slow receiver lags
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        next += n as usize;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            ffs::assert(next == ROUNDS, &format!("receiver {r}: {next} of {ROUNDS} values accounted for"));
        }));
    }
    drop(_rx0);
    for v in 0..ROUNDS {
        tx.send(v).unwrap();
        if v % 3 == 0 {
            tokio::task::yield_now().await;
        }
    }
    drop(tx);
    for h in hs {
        h.await.unwrap();
    }
}

async fn mpsc_order() {
    let (tx, mut rx) = mpsc::channel::<(usize, usize)>(2);
    let mut hs = Vec::new();
    for s in 0..3 {
        let tx = tx.clone();
        hs.push(tokio::spawn(async move {
            for i in 0..ROUNDS {
                if i % 2 == 0 {
                    tx.send((s, i)).await.unwrap();
                } else {
                    let permit = tx.reserve().await.unwrap();
                    permit.send((s, i));
                }
            }
        }));
    }
    drop(tx);
    let mut next = [0usize; 3];
    let mut got = 0;
    let mut buf = Vec::new();
    loop {
        let n = rx.recv_many(&mut buf, 3).await;
        if n == 0 {
            break;
        }
        for (s, i) in buf.drain(..) {
            ffs::assert(i == next[s], &format!("sender {s}: got {i}, expected {}", next[s]));
            next[s] = i + 1;
            got += 1;
        }
    }
    for h in hs {
        h.await.unwrap();
    }
    ffs::assert(got == 3 * ROUNDS, &format!("mpsc delivered {got} of {}", 3 * ROUNDS));
}

async fn watch_last() {
    let (tx, rx) = watch::channel(0usize);
    let mut hs = Vec::new();
    for _ in 0..2 {
        let mut rx = rx.clone();
        hs.push(tokio::spawn(async move {
            let mut last = 0;
            while rx.changed().await.is_ok() {
                let v = *rx.borrow_and_update();
                ffs::assert(v >= last, "watch went backwards");
                last = v;
            }
            last
        }));
    }
    drop(rx);
    for v in 1..=ROUNDS {
        tx.send(v).unwrap();
        if v % 2 == 0 {
            tokio::task::yield_now().await;
        }
    }
    drop(tx);
    for h in hs {
        let last = h.await.unwrap();
        ffs::assert(last == ROUNDS, &format!("watch receiver ended at {last}, not {ROUNDS}"));
    }
}
