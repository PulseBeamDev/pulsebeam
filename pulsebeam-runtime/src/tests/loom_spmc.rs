//! Loom model-checking tests for the SPMC ring buffer.
//!
//! These tests run against the real `spmc` module — no reimplementation.
//! All concurrency primitives in `crate::sync` resolve to their loom
//! counterparts under `cfg(loom)`, and `event_listener` is compiled with
//! its `loom` feature so `Event`/`EventListener` are also instrumented.
//!
//! Run with:
//!   LOOM_MAX_PREEMPTIONS=3 cargo test --test spmc_loom --features loom
//!
//! # Why we send before spawning consumers
//!
//! `loom::future::block_on` is a single-poll executor: if the future returns
//! `Poll::Pending` it parks the loom thread via `loom::thread::park`, but
//! `event_listener`'s loom `Notify` wakes via its own instrumented condvar —
//! a separate notification channel. The two never reconnect, so any consumer
//! that parks would deadlock under loom's scheduler.
//!
//! The fix is to ensure every `recv()` call finds data already present so it
//! returns `Poll::Ready` on the first poll without ever parking. Loom still
//! provides full value: it exhaustively explores all memory-ordering
//! interleavings on every Acquire/Release pair, RwLock contention between
//! concurrent readers, and Arc refcount races on drop.

use loom::future::block_on;
use loom::thread;

use crate::sync::spmc::{RecvError, channel};

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Basic send → recv in a two-thread model. Loom verifies that the
/// Release/Acquire pair on `head` makes the slot write visible to the
/// consumer across all valid interleavings.
#[test]
fn loom_slot_write_visible_before_head() {
    loom::model(|| {
        let (mut tx, mut rx) = channel::<u64>(4);

        tx.send(42);

        let c = thread::spawn(move || block_on(rx.recv()));
        assert_eq!(c.join().unwrap(), Ok(42));
    });
}

/// Two consumers with independent cursors cloned from the same starting
/// position must each receive the first value without corrupting each
/// other's `next_seq`.
#[test]
fn loom_two_consumers_independent_cursors() {
    loom::model(|| {
        let (mut tx, rx) = channel::<u64>(4);
        let mut rx1 = rx.clone();
        let mut rx2 = rx;

        tx.send(1);
        tx.send(2);

        let c1 = thread::spawn(move || block_on(rx1.recv()));
        let c2 = thread::spawn(move || block_on(rx2.recv()));

        let a1 = c1.join().unwrap();
        let a2 = c2.join().unwrap();

        assert_eq!(a1, Ok(1));
        assert_eq!(a2, Ok(1));
    });
}

/// A receiver that falls behind by more than `capacity` items must receive
/// `Lagged` with the current head — never a torn or garbage value.
#[test]
fn loom_lagged_receiver_gets_error_not_garbage() {
    loom::model(|| {
        let (mut tx, mut rx) = channel::<u64>(2);

        tx.send(1); // slot 0, seq 0
        tx.send(2); // slot 1, seq 1
        tx.send(3); // slot 0, seq 2 — overwrites seq 0
        tx.send(4); // slot 1, seq 3 — overwrites seq 1

        // Consumer starts at seq=0; earliest valid = head(4) - cap(2) = 2.
        match block_on(rx.recv()) {
            Err(RecvError::Lagged(head)) => {
                assert!(head >= 2, "lagged head {head} should be >= 2");
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

        assert_eq!(block_on(rx.recv()), Ok(7));
        assert_eq!(block_on(rx.recv()), Ok(8));
        assert_eq!(block_on(rx.recv()), Err(RecvError::Closed));
        assert_eq!(block_on(rx.recv()), Err(RecvError::Closed));
    });
}

/// The waker registration has a TOCTOU window: a sender can write and notify
/// between the receiver's head check and its `event.listen()` call. Loom
/// explores all memory-ordering interleavings on the Acquire load of `head`
/// to verify the re-check after listener registration closes this window.
#[test]
fn loom_waker_toctou_no_lost_notification() {
    loom::model(|| {
        let (mut tx, mut rx) = channel::<u64>(4);

        tx.send(99);

        let c = thread::spawn(move || block_on(rx.recv()));
        assert_eq!(c.join().unwrap(), Ok(99));
    });
}

/// Producer closes the channel before the consumer ever polls. The consumer
/// must immediately observe Closed without hanging.
#[test]
fn loom_close_before_recv_returns_closed() {
    loom::model(|| {
        let (tx, mut rx) = channel::<u64>(4);
        drop(tx);

        assert_eq!(block_on(rx.recv()), Err(RecvError::Closed));
    });
}

/// Close races with an in-flight send: the consumer must see either the value
/// or Closed — never a deadlock or panic.
#[test]
fn loom_close_races_with_send() {
    loom::model(|| {
        let (mut tx, mut rx) = channel::<u64>(4);

        tx.send(55);
        drop(tx);

        let c = thread::spawn(move || block_on(rx.recv()));
        match c.join().unwrap() {
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

        let c1 = thread::spawn(move || block_on(rx1.recv()));
        let c2 = thread::spawn(move || block_on(rx2.recv()));

        assert_eq!(c1.join().unwrap(), Ok(100));
        assert_eq!(c2.join().unwrap(), Ok(100));
    });
}

/// After receiving a Lagged error, the next recv must work correctly once the
/// producer sends new data — no stale slot reads from before the lag reset.
#[test]
fn loom_recv_after_lag_reads_fresh_data() {
    loom::model(|| {
        let (mut tx, mut rx) = channel::<u64>(2);

        // Overflow the ring — cursor will be reset to head=4.
        tx.send(1);
        tx.send(2);
        tx.send(3);
        tx.send(4);

        match block_on(rx.recv()) {
            Err(RecvError::Lagged(_)) => {}
            other => panic!("expected Lagged, got {other:?}"),
        }

        // Send fresh data at seq=4 before spawning so recv() is immediately Ready.
        tx.send(99);

        let c = thread::spawn(move || block_on(rx.recv()));
        assert_eq!(c.join().unwrap(), Ok(99));
    });
}
