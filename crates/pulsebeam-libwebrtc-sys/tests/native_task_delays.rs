use pulsebeam_webrtc_sys::*;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

struct Dropped(Arc<AtomicUsize>);
impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn default_queue_preserves_signed_infinite_and_precise_delays() {
    let factory = TaskQueueFactory::default_threaded().unwrap();
    let mut queue = factory
        .create_queue("native-delays", QueuePriority::Normal)
        .unwrap();
    let (tx, rx) = mpsc::channel();
    for bits in [i64::MIN, -1, 0, 1] {
        for precision in [TaskDelayPrecision::Low, TaskDelayPrecision::High] {
            let tx = tx.clone();
            assert!(
                queue
                    .post_delayed_native(
                        TimeDelta::from_native_microsecond_bits(bits),
                        precision,
                        TaskQueueLocation,
                        move || tx.send(bits).unwrap()
                    )
                    .unwrap()
            );
            assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), bits);
        }
    }
    let dropped = Arc::new(AtomicUsize::new(0));
    // Observe real pending-task retirement, not a binding-imposed wait horizon.
    for bits in [60_000_000, i64::MAX] {
        let marker = Dropped(dropped.clone());
        assert!(
            queue
                .post_delayed_native(
                    TimeDelta::from_native_microsecond_bits(bits),
                    TaskDelayPrecision::High,
                    TaskQueueLocation,
                    move || {
                        drop(marker);
                        panic!("long delay must not run");
                    }
                )
                .unwrap()
        );
    }
    assert_eq!(dropped.load(Ordering::SeqCst), 0);
    let (tx, rx) = mpsc::channel();
    assert!(queue.post(move || {
        let context = TaskQueueContext::current().unwrap();
        assert_eq!(context.name_bytes(), b"native-delays");
        context
            .post_delayed(
                TimeDelta::minus_infinity(),
                TaskDelayPrecision::High,
                TaskQueueLocation,
                move || tx.send(()).unwrap(),
            )
            .unwrap();
    }));
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    queue.close();
    assert_eq!(dropped.load(Ordering::SeqCst), 2);
}

#[test]
fn cooperative_queue_accepts_finite_states_without_autonomous_progress() {
    let clock = ManualClock::new(Duration::from_secs(10)).unwrap();
    let factory = TaskQueueFactory::cooperative(&clock).unwrap();
    let mut queue = factory
        .create_queue("finite", QueuePriority::Normal)
        .unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    for bits in [i64::MIN + 1, -1, 0, 2] {
        let count = count.clone();
        assert!(
            queue
                .post_delayed_native(
                    TimeDelta::from_native_microsecond_bits(bits),
                    TaskDelayPrecision::High,
                    TaskQueueLocation,
                    move || {
                        count.fetch_add(1, Ordering::SeqCst);
                    }
                )
                .unwrap()
        );
    }
    for delay in [TimeDelta::plus_infinity(), TimeDelta::minus_infinity()] {
        assert_eq!(
            queue.post_delayed_native(delay, TaskDelayPrecision::Low, TaskQueueLocation, || {}),
            Err(BuildEnvironmentError::UnsupportedNativeDelay)
        );
    }
    let discarded = Arc::new(AtomicUsize::new(0));
    let marker = Dropped(discarded.clone());
    // Native void posting silently discards an out-of-domain deadline.
    assert!(
        queue
            .post_delayed_native(
                TimeDelta::from_microseconds(i64::MAX - 1).unwrap(),
                TaskDelayPrecision::Low,
                TaskQueueLocation,
                move || drop(marker)
            )
            .unwrap()
    );
    assert_eq!(discarded.load(Ordering::SeqCst), 1);
    assert_eq!(count.load(Ordering::SeqCst), 0);
    assert_eq!(factory.pump(20).unwrap().dispatched, 3);
    assert_eq!(count.load(Ordering::SeqCst), 3);
    clock.advance(Duration::from_micros(2)).unwrap();
    assert_eq!(factory.pump(20).unwrap().dispatched, 1);
    let count_copy = count.clone();
    assert!(queue.post(move || {
        let current = TaskQueueContext::current().unwrap();
        assert_eq!(
            current.post_delayed(
                TimeDelta::plus_infinity(),
                TaskDelayPrecision::Low,
                TaskQueueLocation,
                || {}
            ),
            Err(TaskQueuePostError::UnsupportedNativeDelay)
        );
        current
            .post_delayed(
                TimeDelta::from_microseconds(-1).unwrap(),
                TaskDelayPrecision::High,
                TaskQueueLocation,
                move || {
                    count_copy.fetch_add(1, Ordering::SeqCst);
                },
            )
            .unwrap();
    }));
    assert_eq!(factory.pump(20).unwrap().dispatched, 2);
    assert_eq!(count.load(Ordering::SeqCst), 5);
    queue.close();
}

#[test]
fn thread_backend_posts_native_delays_and_retires_pending_work() {
    let mut worker = WorkerThread::start().unwrap();
    let (tx, rx) = mpsc::channel();
    for bits in [-1998, -1, 0, 1, 1001] {
        for precision in [TaskDelayPrecision::Low, TaskDelayPrecision::High] {
            let tx = tx.clone();
            assert!(
                worker
                    .post_delayed_native(
                        TimeDelta::from_microseconds(bits).unwrap(),
                        precision,
                        TaskQueueLocation,
                        move || tx.send(bits).unwrap()
                    )
                    .unwrap()
            );
            assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), bits);
        }
    }
    let dropped = Arc::new(AtomicUsize::new(0));
    let marker = Dropped(dropped.clone());
    assert!(
        worker
            .post_delayed_native(
                TimeDelta::from_microseconds(60_000_000).unwrap(),
                TaskDelayPrecision::High,
                TaskQueueLocation,
                move || {
                    drop(marker);
                    panic!("long delay must not run");
                }
            )
            .unwrap()
    );
    let (tx, rx) = mpsc::channel();
    assert!(worker.post(move || {
        let context = TaskQueueContext::current().unwrap();
        context
            .post_delayed(
                TimeDelta::from_microseconds(-1998).unwrap(),
                TaskDelayPrecision::Low,
                TaskQueueLocation,
                move || tx.send(()).unwrap(),
            )
            .unwrap();
    }));
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    worker.close();
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}
