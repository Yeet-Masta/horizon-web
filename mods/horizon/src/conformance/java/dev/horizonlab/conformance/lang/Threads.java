package dev.horizonlab.conformance.lang;

import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.Callable;
import java.util.concurrent.CompletableFuture;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutionException;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;
import java.util.concurrent.atomic.AtomicReference;
import java.util.concurrent.locks.ReentrantLock;

/** Threads and synchronization, written so the output does not depend on scheduling: start/join,
 * synchronized counters, wait/notify hand-offs, executors and futures read in submission order,
 * CompletableFuture chains, latches, atomics, locks and thread-locals. */
public class Threads {
    private int counter;
    private final Object lock = new Object();
    private final List<Integer> buffer = new ArrayList<>();

    synchronized void increment() {
        counter++;
    }

    void produce(int value) throws InterruptedException {
        synchronized (lock) {
            while (!buffer.isEmpty()) {
                lock.wait();
            }
            buffer.add(value);
            lock.notifyAll();
        }
    }

    int consume() throws InterruptedException {
        synchronized (lock) {
            while (buffer.isEmpty()) {
                lock.wait();
            }
            int value = buffer.remove(0);
            lock.notifyAll();
            return value;
        }
    }

    static int fib(int n) {
        return n < 2 ? n : fib(n - 1) + fib(n - 2);
    }

    public static void main(String[] args) throws Exception {
        System.out.println("main thread " + Thread.currentThread().getName() + " alive " + Thread.currentThread().isAlive());
        Threads shared = new Threads();
        List<Thread> threads = new ArrayList<>();
        for (int t = 0; t < 4; t++) {
            Thread thread = new Thread(() -> {
                for (int i = 0; i < 1000; i++) {
                    shared.increment();
                }
            }, "worker-" + t);
            threads.add(thread);
        }
        for (Thread thread : threads) {
            thread.start();
        }
        for (Thread thread : threads) {
            thread.join();
        }
        System.out.println("synchronized counter " + shared.counter + " names " + threads.get(3).getName() + " state "
                + threads.get(0).getState());

        int[] results = new int[3];
        Thread[] workers = new Thread[3];
        for (int i = 0; i < 3; i++) {
            final int index = i;
            workers[i] = new Thread(() -> results[index] = fib(15 + index));
            workers[i].start();
        }
        for (Thread worker : workers) {
            worker.join();
        }
        System.out.println("join results " + results[0] + " " + results[1] + " " + results[2]);

        Thread producer = new Thread(() -> {
            try {
                for (int i = 1; i <= 5; i++) {
                    shared.produce(i * i);
                }
                shared.produce(-1);
            } catch (InterruptedException e) {
                Thread.currentThread().interrupt();
            }
        });
        producer.start();
        StringBuilder consumed = new StringBuilder();
        for (int v = shared.consume(); v != -1; v = shared.consume()) {
            consumed.append(v).append(' ');
        }
        producer.join();
        System.out.println("wait/notify " + consumed.toString().trim());

        ExecutorService pool = Executors.newFixedThreadPool(3);
        List<Future<Integer>> futures = new ArrayList<>();
        for (int i = 0; i < 6; i++) {
            final int n = i;
            futures.add(pool.submit(() -> fib(10 + n)));
        }
        StringBuilder fromFutures = new StringBuilder();
        for (Future<Integer> future : futures) {
            fromFutures.append(future.get()).append(' ');
        }
        System.out.println("executor " + fromFutures.toString().trim());
        Callable<String> failing = () -> {
            throw new IllegalStateException("task failed");
        };
        try {
            pool.submit(failing).get();
        } catch (ExecutionException e) {
            System.out.println("ExecutionException cause " + e.getCause().getMessage());
        }
        List<Callable<Integer>> batch = List.of(() -> 1, () -> 2, () -> 3);
        int sum = 0;
        for (Future<Integer> f : pool.invokeAll(batch)) {
            sum += f.get();
        }
        System.out.println("invokeAll " + sum);
        pool.shutdown();
        System.out.println("terminated " + pool.awaitTermination(10, TimeUnit.SECONDS) + " " + pool.isShutdown());

        CompletableFuture<String> chain = CompletableFuture.supplyAsync(() -> 6)
                .thenApply(x -> x * 7)
                .thenCompose(x -> CompletableFuture.supplyAsync(() -> x + 1))
                .thenCombine(CompletableFuture.completedFuture("!"), (x, s) -> x + s);
        System.out.println("CompletableFuture " + chain.join() + " " + chain.isDone());
        CompletableFuture<Integer> failed = CompletableFuture.supplyAsync(() -> {
            throw new IllegalArgumentException("boom");
        });
        System.out.println("exceptionally " + failed.exceptionally(t -> -1).join() + " handle "
                + failed.handle((v, t) -> t.getCause().getMessage()).join());
        CompletableFuture<Void> all = CompletableFuture.allOf(CompletableFuture.runAsync(() -> {}),
                CompletableFuture.runAsync(() -> {}));
        all.get(5, TimeUnit.SECONDS);
        System.out.println("allOf done " + all.isDone());

        CountDownLatch latch = new CountDownLatch(3);
        AtomicInteger atomic = new AtomicInteger();
        for (int i = 0; i < 3; i++) {
            new Thread(() -> {
                atomic.addAndGet(10);
                latch.countDown();
            }).start();
        }
        latch.await();
        System.out.println("latch " + latch.getCount() + " atomic " + atomic.get() + " " + atomic.incrementAndGet() + " "
                + atomic.compareAndSet(31, 40) + " " + atomic.getAndUpdate(x -> x * 2) + " " + atomic.accumulateAndGet(5, Integer::sum));
        AtomicLong along = new AtomicLong(1L << 40);
        AtomicReference<String> ref = new AtomicReference<>("a");
        ref.updateAndGet(s -> s + "b");
        System.out.println("AtomicLong " + along.getAndIncrement() + " AtomicReference " + ref.get() + " "
                + ref.compareAndSet("x", "y"));

        ReentrantLock reentrant = new ReentrantLock();
        int[] guarded = new int[1];
        Thread[] lockers = new Thread[4];
        for (int i = 0; i < lockers.length; i++) {
            lockers[i] = new Thread(() -> {
                for (int k = 0; k < 500; k++) {
                    reentrant.lock();
                    try {
                        guarded[0]++;
                    } finally {
                        reentrant.unlock();
                    }
                }
            });
            lockers[i].start();
        }
        for (Thread locker : lockers) {
            locker.join();
        }
        System.out.println("ReentrantLock " + guarded[0] + " held " + reentrant.isLocked());

        ConcurrentHashMap<String, Integer> chm = new ConcurrentHashMap<>();
        Thread[] mergers = new Thread[4];
        for (int i = 0; i < mergers.length; i++) {
            mergers[i] = new Thread(() -> {
                for (int k = 0; k < 100; k++) {
                    chm.merge("k" + (k % 5), 1, Integer::sum);
                }
            });
            mergers[i].start();
        }
        for (Thread merger : mergers) {
            merger.join();
        }
        System.out.println("ConcurrentHashMap " + new java.util.TreeMap<>(chm));
        CopyOnWriteArrayList<Integer> cow = new CopyOnWriteArrayList<>(List.of(1, 2, 3));
        for (Integer v : cow) {
            cow.add(v * 10);
        }
        System.out.println("CopyOnWriteArrayList " + cow);

        ThreadLocal<String> local = ThreadLocal.withInitial(() -> "initial");
        local.set("main value");
        String[] seen = new String[1];
        Thread reader = new Thread(() -> seen[0] = local.get());
        reader.start();
        reader.join();
        System.out.println("ThreadLocal " + local.get() + " / " + seen[0]);

        Thread sleeper = new Thread(() -> {
            try {
                Thread.sleep(10_000);
                System.out.println("not interrupted");
            } catch (InterruptedException e) {
                System.out.println("interrupted while sleeping");
            }
        });
        sleeper.start();
        sleeper.interrupt();
        sleeper.join();
        Thread daemon = new Thread(() -> {});
        daemon.setDaemon(true);
        System.out.println("daemon " + daemon.isDaemon() + " priority " + daemon.getPriority());
        Thread.sleep(5);
        Object monitor = new Object();
        synchronized (monitor) {
            synchronized (monitor) {
                System.out.println("reentrant monitor " + Thread.holdsLock(monitor));
            }
        }
        try {
            monitor.notify();
        } catch (IllegalMonitorStateException e) {
            System.out.println("IllegalMonitorStateException");
        }
        Thread uncaught = new Thread(() -> {
            throw new RuntimeException("in thread");
        });
        uncaught.setUncaughtExceptionHandler((t, e) -> System.out.println("handler got " + e.getMessage()));
        uncaught.start();
        uncaught.join();
        System.out.println("done");
    }
}
