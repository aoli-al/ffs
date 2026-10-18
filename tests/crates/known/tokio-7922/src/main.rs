//! tokio PR #7922: race between a blocking thread's wakeup and the blocking
//! pool's shutdown double-decrements `num_idle_threads`, which panics the
//! blocking thread ("`num_idle_threads` underflowed on thread exit").
//! This is the loom test `spawn_blocking_then_shutdown` on real threads.
//! A panic in a blocking thread is swallowed by the pool, so the exit code
//! is decided by a panic hook.
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

static PANICKED: AtomicBool = AtomicBool::new(false);

fn main() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        PANICKED.store(true, Ordering::SeqCst);
        prev(info);
    }));
    let rt = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .thread_keep_alive(Duration::from_secs(7200))
        .build()
        .unwrap();
    let hdl = rt.handle().clone();
    // Creates the single blocking thread; it then parks on the pool's condvar.
    let jh0 = hdl.spawn_blocking(|| {});
    rt.block_on(jh0).unwrap();
    // Decrements num_idle_threads and notifies the parked thread ...
    let jh3 = hdl.spawn_blocking(|| {});
    // ... while the shutdown flips `shutdown` and joins it.
    drop(rt);
    let _ = futures_lite_block_on(jh3);
    if PANICKED.load(Ordering::SeqCst) {
        eprintln!("FAIL: a thread panicked");
        std::process::exit(101);
    }
    println!("ok");
}

fn futures_lite_block_on<F: std::future::Future>(fut: F) -> F::Output {
    use std::task::{Context, Poll, Wake, Waker};
    use std::sync::Arc;
    struct Noop;
    impl Wake for Noop {
        fn wake(self: Arc<Self>) {}
    }
    let waker = Waker::from(Arc::new(Noop));
    let mut cx = Context::from_waker(&waker);
    let mut fut = Box::pin(fut);
    loop {
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::yield_now();
    }
}
