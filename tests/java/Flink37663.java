import java.lang.reflect.Field;
import java.util.Queue;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import java.util.concurrent.locks.Lock;
import org.apache.flink.connector.base.source.reader.synchronization.FutureCompletingBlockingQueue;

/** Isolated regression using the actual Flink queue, not a reimplementation. */
public final class Flink37663 {
    static final FutureCompletingBlockingQueue<Integer> queue = new FutureCompletingBlockingQueue<>(1);
    static final AtomicReference<Throwable> error = new AtomicReference<>();
    static final CountDownLatch aDone = new CountDownLatch(1);
    static final CountDownLatch bDone = new CountDownLatch(1);
    static volatile boolean aInserted;
    static final long TIMEOUT = 10;

    static int queuedPutters() throws Exception {
        Field lf = queue.getClass().getDeclaredField("lock");
        Field nf = queue.getClass().getDeclaredField("notFull");
        lf.setAccessible(true);
        nf.setAccessible(true);
        Lock lock = (Lock) lf.get(queue);
        lock.lock();
        try { return ((Queue<?>) nf.get(queue)).size(); }
        finally { lock.unlock(); }
    }

    static void awaitQueued(int count) throws Exception {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(TIMEOUT);
        while (queuedPutters() < count && System.nanoTime() < deadline) {
            Thread.sleep(1);
        }
        Ffs.check(queuedPutters() == count, "setup: producers did not reach condition wait");
    }

    static Thread putter(int id, CountDownLatch done) {
        Thread thread = new Thread(() -> {
            try {
                boolean inserted = queue.put(id, id + 1);
                if (id == 0) aInserted = inserted;
                else Ffs.check(inserted, "producer B should insert into freed slot");
            } catch (Throwable t) { error.compareAndSet(null, t); }
            finally { done.countDown(); }
        }, "putter-" + id);
        thread.setDaemon(true);
        thread.start();
        return thread;
    }

    public static void main(String[] args) throws Exception {
        queue.put(0, 0);
        Thread a = putter(0, aDone);
        awaitQueued(1);
        Thread b = putter(1, bDone);
        awaitQueued(2);
        // Both producers and JVM service threads exist at the checkpoint.
        Ffs.begin();
        queue.wakeUpPuttingThread(0);
        boolean sequenced = args.length > 0 && args[0].equals("sequenced");
        if (sequenced) Ffs.check(aDone.await(TIMEOUT, TimeUnit.SECONDS), "producer A failed to wake");
        else Ffs.yield(); // Explore whether A returns before the consumer frees space.
        Ffs.check(queue.poll() == 0, "consumer did not remove initial element");
        Ffs.check(aDone.await(TIMEOUT, TimeUnit.SECONDS), "producer A failed to wake");
        Ffs.check(error.get() == null, "producer A failed");
        // If A raced ahead and filled the slot, consume its element too. B
        // cannot insert until this second poll, so this is not an extra wakeup
        // masking the bug when A instead returned false.
        if (aInserted) Ffs.check(queue.poll() == 1, "missing producer A element");
        boolean completed = bDone.await(TIMEOUT, TimeUnit.SECONDS);
        if (!completed) System.out.println("FLINK-37663: B stranded; queued=" + queuedPutters() + ", size=" + queue.size());
        Ffs.check(completed, "FLINK-37663: producer B stranded after consumer freed a slot");
        Ffs.check(error.get() == null, "producer B failed");
        a.join();
        b.join();
        Ffs.check(queue.poll() == 2, "missing producer B element");
        Ffs.end();
        System.out.println("PASS: both producers completed");
    }
}
