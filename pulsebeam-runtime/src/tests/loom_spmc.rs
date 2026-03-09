//! Loom model-checking tests for the SPMC ring buffer.
//!
//! These tests run against the real `spmc` module — no reimplementation.
//! All concurrency primitives in `crate::sync` resolve to their loom
//! counterparts under `cfg(loom)`, and `event_listener` is compiled with
//! its `loom` feature so `Event`/`EventListener` are also instrumented.
//!
//! Run with:
//!   LOOM_MAX_PREEMPTIONS=3 cargo test --test spmc_loom --features loom

use loom::future::block_on;
use loom::thread;

use crate::sync::spmc::{RecvError, channel};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Drive a single `recv()` call to completion inside `loom::future::block_on`.
/// Because we're inside loom, there is no Tokio runtime — `block_on` is the
/// executor.  Each call is single-threaded within the future itself; concurrency
/// comes from `loom::thread::spawn` around it.
macro_rules! recv {
    ($rx:expr) => {
        block_on($rx.recv())
    };
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Basic send → recv in a two-thread model.  Loom verifies that the
/// Release/Acquire pair on `head` makes the slot write visible to the
/// consumer across all valid interleavings.
#[test]
fn loom_slot_write_visible_before_head() {
    loom::model(|| {
        let (mut tx, mut rx) = channel::<u64>(4);

        let producer = thread::spawn(move || {
            tx.send(42);
        });

        let result = recv!(rx);

        producer.join().unwrap();

        assert_eq!(result, Ok(42));
    });
}

/// Two consumers with independent cursors cloned from the same starting
/// position must each receive all values without corrupting each other's
/// `next_seq`.
#[test]
fn loom_two_consumers_independent_cursors() {
    loom::model(|| {
        let (mut tx, rx) = channel::<u64>(4);
        let mut rx1 = rx.clone();
        let mut rx2 = rx;

        let producer = thread::spawn(move || {
            tx.send(1);
            tx.send(2);
        });

        let c1 = thread::spawn(move || {
            let a = recv!(rx1);
            let b = recv!(rx1);
            (a, b)
        });

        let c2 = thread::spawn(move || {
            let a = recv!(rx2);
            let b = recv!(rx2);
            (a, b)
        });

        producer.join().unwrap();
        let (a1, b1) = c1.join().unwrap();
        let (a2, b2) = c2.join().unwrap();

        assert_eq!((a1, b1), (Ok(1), Ok(2)));
        assert_eq!((a2, b2), (Ok(1), Ok(2)));
    });
}

/// A receiver that falls behind by more than `capacity` items must receive
/// `Lagged` with the current head — never a torn or garbage value.
#[test]
fn loom_lagged_receiver_gets_error_not_garbage() {
    loom::model(|| {
        let (mut tx, mut rx) = channel::<u64>(2);

        // Fill and overflow the ring completely before the consumer reads.
        tx.send(1); // slot 0, seq 0
        tx.send(2); // slot 1, seq 1
        tx.send(3); // slot 0, seq 2 — overwrites seq 0
        tx.send(4); // slot 1, seq 3 — overwrites seq 1

        // Consumer starts at seq=0; earliest valid = head(4) - cap(2) = 2.
        let result = recv!(rx);

        match result {
            Err(RecvError::Lagged(head)) => {
                assert!(head >= 2, "lagged head {head} should be >= 2");
                // After lag the cursor is reset to head — a subsequent recv
                // must not return a phantom stale value.
                // assert_eq!(rx.next_seq, rx.local_head);
            }
            other => panic!("expected Lagged, got {other:?}"),
        }
    });
}

/// The closed signal must only be observed after all buffered items are
/// drained, regardless of the interleaving between the drop and the recvs.
#[test]
fn loom_close_drains_then_stops() {
    loom::model(|| {
        let (mut tx, mut rx) = channel::<u64>(4);

        tx.send(7);
        tx.send(8);
        drop(tx);

        assert_eq!(recv!(rx), Ok(7));
        assert_eq!(recv!(rx), Ok(8));
        assert_eq!(recv!(rx), Err(RecvError::Closed));
        // Idempotent — subsequent calls must also return Closed.
        assert_eq!(recv!(rx), Err(RecvError::Closed));
    });
}

/// The waker registration has a TOCTOU window: a sender can write and notify
/// *between* the receiver's head check and its `event.listen()` call.  The
/// re-check after listener registration must close this window so the receiver
/// doesn't park indefinitely.
///
/// Loom explores the interleaving where the producer sends *during* the
/// listener registration in `poll_recv`.
#[test]
fn loom_waker_toctou_no_lost_notification() {
    loom::model(|| {
        let (mut tx, mut rx) = channel::<u64>(4);

        let producer = thread::spawn(move || {
            tx.send(99);
        });

        // This recv() may race with the send() above.  The re-check after
        // listener registration ensures we never miss the notification.
        let result = recv!(rx);

        producer.join().unwrap();

        assert_eq!(result, Ok(99));
    });
}

/// Producer closes the channel *before* the consumer ever polls.  The consumer
/// must immediately observe Closed without hanging.
#[test]
fn loom_close_before_recv_returns_closed() {
    loom::model(|| {
        let (tx, mut rx) = channel::<u64>(4);
        drop(tx);

        assert_eq!(recv!(rx), Err(RecvError::Closed));
    });
}

/// Close races with an in-flight send: the consumer must see either the value
/// or Closed — never a deadlock or panic.
#[test]
fn loom_close_races_with_send() {
    loom::model(|| {
        let (mut tx, mut rx) = channel::<u64>(4);

        let producer = thread::spawn(move || {
            tx.send(55);
            // drop(tx) triggers closed store + notify
        });

        let result = recv!(rx);

        producer.join().unwrap();

        // Either we got the value or the channel closed — both are valid
        // depending on the interleaving.
        match result {
            Ok(55) => {}
            Err(RecvError::Closed) => {}
            other => panic!("unexpected result: {other:?}"),
        }
    });
}

/// Two consumers racing on the same slot: RwLock allows concurrent reads, and
/// loom must find no data race between them.
#[test]
fn loom_concurrent_reads_no_data_race() {
    loom::model(|| {
        let (mut tx, rx) = channel::<u64>(4);
        let mut rx1 = rx.clone();
        let mut rx2 = rx;

        tx.send(100);

        let c1 = thread::spawn(move || recv!(rx1));
        let c2 = thread::spawn(move || recv!(rx2));

        let r1 = c1.join().unwrap();
        let r2 = c2.join().unwrap();

        assert_eq!(r1, Ok(100));
        assert_eq!(r2, Ok(100));
    });
}

/// After receiving a Lagged error, the next recv must work correctly once the
/// producer sends new data — no stale slot reads from before the lag reset.
#[test]
fn loom_recv_after_lag_reads_fresh_data() {
    loom::model(|| {
        let (mut tx, mut rx) = channel::<u64>(2);

        // Overflow the ring.
        tx.send(1);
        tx.send(2);
        tx.send(3); // overwrites slot 0
        tx.send(4); // overwrites slot 1

        // First recv: Lagged — cursor jumps to head=4.
        match recv!(rx) {
            Err(RecvError::Lagged(_)) => {}
            other => panic!("expected Lagged, got {other:?}"),
        }

        // Producer sends fresh data at seq=4.
        let producer = thread::spawn(move || {
            tx.send(99);
        });

        let result = recv!(rx);

        producer.join().unwrap();

        assert_eq!(result, Ok(99));
    });
}
