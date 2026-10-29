#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::arithmetic_side_effects,
    clippy::indexing_slicing,
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "tests assert contracts and use host threads only for real concurrency"
)]

use super::*;
use futures_lite::future::{block_on, poll_once};
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

#[derive(Default)]
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn exact_capacity_readiness_ownership_batch_and_drain() {
    // Non-power-of-two capacities exercise ring wrap as well as capacity one.
    for capacity in [1, 2, 3, 1024] {
        let (tx, mut rx) = bounded(NonZeroUsize::new(capacity).unwrap());
        for round in 0..3 {
            for value in 0..capacity {
                tx.try_send(Box::new(value)).unwrap();
            }
            for _ in 0..3 {
                assert!(block_on(rx.readable()));
                let rejected = Box::new(capacity);
                let address = &*rejected as *const usize;
                let Err(TrySendError::Full(rejected)) = tx.try_send(rejected) else {
                    panic!("readiness must not free capacity");
                };
                assert_eq!(address, &*rejected as *const usize);
            }
            let mut values = vec![Box::new(usize::MAX)];
            assert_eq!(rx.try_recv_batch(&mut values, 0), 0);
            assert_eq!(values.len(), 1);
            assert_eq!(rx.try_recv_batch(&mut values, 1), 1);
            assert_eq!(rx.try_recv_batch(&mut values, capacity), capacity - 1);
            assert_eq!(
                values,
                std::iter::once(usize::MAX)
                    .chain(0..capacity)
                    .map(Box::new)
                    .collect::<Vec<_>>()
            );
            assert_eq!(rx.try_recv(), Err(TryRecvError::Empty));
            tx.try_send(Box::new(round)).unwrap();
            assert_eq!(block_on(rx.recv()), Some(Box::new(round)));
        }
        let clone = tx.clone();
        drop(tx);
        clone.try_send(Box::new(42)).unwrap();
        drop(clone);
        assert!(block_on(rx.readable()));
        assert_eq!(rx.try_recv(), Ok(Box::new(42)));
        assert!(!block_on(rx.readable()));
        assert_eq!(block_on(rx.recv()), None);
        assert_eq!(rx.try_recv(), Err(TryRecvError::Closed));
    }
}

#[test]
fn cancelled_waits_replace_wakers_without_consuming_or_reordering() {
    let (tx, mut rx) = bounded(NonZeroUsize::MIN);
    let old_wakes = Arc::new(WakeCount::default());
    let new_wakes = Arc::new(WakeCount::default());
    let old_waker = Waker::from(old_wakes);
    let new_waker = Waker::from(new_wakes.clone());
    {
        let mut ready = pin!(rx.readable());
        assert!(
            ready
                .as_mut()
                .poll(&mut Context::from_waker(&old_waker))
                .is_pending()
        );
    }
    {
        let mut ready = pin!(rx.readable());
        assert!(
            ready
                .as_mut()
                .poll(&mut Context::from_waker(&new_waker))
                .is_pending()
        );
        tx.try_send(7).unwrap();
        assert!(new_wakes.0.load(Ordering::Relaxed) > 0);
        assert_eq!(
            ready.as_mut().poll(&mut Context::from_waker(&new_waker)),
            Poll::Ready(true)
        );
    }
    // Cancellation after readiness still cannot consume or release a slot.
    assert_eq!(tx.try_send(8), Err(TrySendError::Full(8)));
    assert_eq!(block_on(rx.recv()), Some(7));
    {
        let mut receive = pin!(rx.recv());
        assert_eq!(block_on(poll_once(&mut receive)), None);
    }
    tx.try_send(8).unwrap();
    assert_eq!(rx.try_recv(), Ok(8));
    {
        let mut ready = pin!(rx.readable());
        assert!(
            ready
                .as_mut()
                .poll(&mut Context::from_waker(&new_waker))
                .is_pending()
        );
        let before = new_wakes.0.load(Ordering::Relaxed);
        drop(tx);
        assert!(new_wakes.0.load(Ordering::Relaxed) > before);
        assert_eq!(
            ready.as_mut().poll(&mut Context::from_waker(&new_waker)),
            Poll::Ready(false)
        );
    }
}

#[derive(Debug)]
struct Tracked(Arc<AtomicUsize>);
impl Drop for Tracked {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn control_send_cancellation_space_and_receiver_drop() {
    let drops = Arc::new(AtomicUsize::new(0));
    let (tx, mut rx) = bounded(NonZeroUsize::MIN);
    tx.try_send(Tracked(drops.clone())).unwrap();
    {
        let mut send = Box::pin(tx.send(Tracked(drops.clone())));
        assert!(block_on(poll_once(&mut send)).is_none());
    }
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert!(rx.try_recv().is_ok());
    assert_eq!(drops.load(Ordering::Relaxed), 2);
    assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));

    tx.try_send(Tracked(drops.clone())).unwrap();
    let mut send = Box::pin(tx.send(Tracked(drops.clone())));
    let wakes = Arc::new(WakeCount::default());
    let waker = Waker::from(wakes.clone());
    assert!(
        send.as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    drop(rx.try_recv().unwrap());
    assert!(wakes.0.load(Ordering::Relaxed) > 0);
    assert!(matches!(
        send.as_mut().poll(&mut Context::from_waker(&waker)),
        Poll::Ready(Ok(()))
    ));
    assert!(rx.try_recv().is_ok());
    drop(send);

    tx.try_send(Tracked(drops.clone())).unwrap();
    let mut send = Box::pin(tx.send(Tracked(drops.clone())));
    assert!(
        send.as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    let before = wakes.0.load(Ordering::Relaxed);
    drop(rx);
    assert!(wakes.0.load(Ordering::Relaxed) > before);
    let Poll::Ready(Err(SendError(value))) = send.as_mut().poll(&mut Context::from_waker(&waker))
    else {
        panic!("receiver drop must return the unaccepted value");
    };
    drop(value);
    assert!(matches!(
        tx.try_send(Tracked(drops.clone())),
        Err(TrySendError::Closed(_))
    ));
    drop(send);
    drop(tx);
    assert_eq!(drops.load(Ordering::Relaxed), 7);
}

#[test]
fn simultaneous_producers_keep_fifo_through_pauses_and_receiver_migration() {
    const PRODUCERS: usize = 4;
    const ITEMS: usize = 512;
    let (tx, mut rx) = bounded(NonZeroUsize::new(3).unwrap());
    let barrier = Arc::new(std::sync::Barrier::new(PRODUCERS + 1));
    std::thread::scope(|scope| {
        for producer in 0..PRODUCERS {
            let tx = tx.clone();
            let barrier = barrier.clone();
            scope.spawn(move || {
                barrier.wait();
                for sequence in 0..ITEMS {
                    let mut value = (producer, sequence);
                    loop {
                        match tx.try_send(value) {
                            Ok(()) => break,
                            Err(TrySendError::Full(returned)) => {
                                value = returned;
                                std::thread::yield_now();
                            }
                            Err(TrySendError::Closed(_)) => panic!("receiver closed early"),
                        }
                    }
                }
            });
        }
        drop(tx);
        barrier.wait();
        // The same receiver is driven sequentially on two real workers, then
        // returned to this worker. Tiny capacity repeatedly stalls producers.
        let mut next = [0; PRODUCERS];
        for _ in 0..2 {
            (rx, next) = scope
                .spawn(move || {
                    for _ in 0..ITEMS {
                        assert!(block_on(rx.readable()));
                        let (producer, sequence) = rx.try_recv().unwrap();
                        assert_eq!(sequence, next[producer]);
                        next[producer] += 1;
                    }
                    (rx, next)
                })
                .join()
                .unwrap();
        }
        while let Some((producer, sequence)) = block_on(rx.recv()) {
            assert_eq!(sequence, next[producer]);
            next[producer] += 1;
        }
        assert_eq!(next, [ITEMS; PRODUCERS]);
        assert!(!block_on(rx.readable()));
    });
}

#[test]
fn send_and_last_sender_drop_race_readiness_registration() {
    for iteration in 0..64 {
        let (tx, mut rx) = bounded(NonZeroUsize::MIN);
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                barrier.wait();
                if iteration % 2 == 0 {
                    tx.try_send(iteration).unwrap();
                }
                drop(tx);
            });
            barrier.wait();
            assert_eq!(block_on(rx.readable()), iteration % 2 == 0);
            if iteration % 2 == 0 {
                assert_eq!(rx.try_recv(), Ok(iteration));
            }
            assert_eq!(block_on(rx.recv()), None);
        });
    }
}

#[tokio::test(start_paused = true)]
async fn virtual_time_select_cancellation_and_full_lane_independence() {
    use std::time::Duration;
    let (full_tx, mut full_rx) = bounded(NonZeroUsize::MIN);
    full_tx.try_send(1).unwrap();
    let (tx, mut rx) = bounded(NonZeroUsize::MIN);
    let producer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(8)).await;
        assert_eq!(full_tx.try_send(2), Err(TrySendError::Full(2)));
        tx.try_send(42).unwrap();
    });
    // The same empty receiver is polled, cancelled and restarted under virtual
    // time while a separate destination remains saturated.
    let mut cancellations = 0;
    loop {
        tokio::select! {
            biased;
            true = rx.readable() => break,
            _ = tokio::time::sleep(Duration::from_millis(1)) => cancellations += 1,
        }
    }
    assert!(cancellations >= 3);
    assert_eq!(rx.try_recv(), Ok(42));
    producer.await.unwrap();
    assert!(!rx.readable().await);
    assert_eq!(full_rx.try_recv(), Ok(1));
    assert_eq!(full_rx.try_recv(), Err(TryRecvError::Closed));
}
