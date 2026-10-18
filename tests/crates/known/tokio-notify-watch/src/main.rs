//! tokio `Notify` permits under a hardware watchpoint on its state word:
//! K `notify_one` calls, one in flight at a time, two waiter tasks that stop once K
//! wakeups have been counted, and a third task mixing in `notify_waiters`
//! on another Notify.  A lost permit leaves the waiters blocked (ffs
//! reports the deadlock); a duplicated one overshoots the count.  The
//! Notify is a `#[no_mangle]` static so the harness can register its first
//! 16 bytes with `ffs_watch` before the checkpoint (`watch` argument).
mod ffs;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

#[no_mangle]
static NOTIFY: Notify = Notify::const_new();
#[no_mangle]
static OTHER: Notify = Notify::const_new();
static WOKEN: AtomicUsize = AtomicUsize::new(0);
const K: usize = 10;

fn main() {
    let watch = std::env::args().nth(1).as_deref() == Some("watch");
    if watch {
        ffs::watch(&NOTIFY as *const Notify as usize, 16);
    }
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).build().unwrap();
    ffs::checkpoint("ready");
    rt.block_on(async {
        let waiters: Vec<_> = (0..2)
            .map(|_| {
                tokio::spawn(async move {
                    loop {
                        if WOKEN.load(Ordering::SeqCst) >= K {
                            return;
                        }
                        NOTIFY.notified().await;
                        let n = WOKEN.fetch_add(1, Ordering::SeqCst) + 1;
                        // K permits plus the release notify below.
                        ffs::assert(n <= K + 1, "more wakeups than notify_one calls: a permit was duplicated");
                        if n == K {
                            // Release the other waiter, which may be parked
                            // on a Notify that will get no more permits.
                            NOTIFY.notify_one();
                            return;
                        }
                    }
                })
            })
            .collect();
        // One notifier, one notification in flight at a time: notify_one
        // coalesces permits when nobody waits, so concurrent notify_one
        // calls could legitimately be lost.
        let notifier = tokio::spawn(async move {
            for j in 0..K {
                NOTIFY.notify_one();
                while WOKEN.load(Ordering::SeqCst) < j + 1 {
                    tokio::task::yield_now().await;
                }
            }
        });
        let mixer = tokio::spawn(async {
            for _ in 0..3 {
                let n = OTHER.notified();
                tokio::pin!(n);
                n.as_mut().enable();
                OTHER.notify_waiters();
                n.await;
                tokio::task::yield_now().await;
            }
        });
        notifier.await.unwrap();
        mixer.await.unwrap();
        for t in waiters {
            t.await.unwrap();
        }
    });
    let w = WOKEN.load(Ordering::SeqCst);
    ffs::assert(w >= K, &format!("only {w} wakeups for {K} notify_one calls"));
    println!("ok {w}");
}
