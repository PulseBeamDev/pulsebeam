#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::disallowed_types,
    reason = "deterministic fixtures and explicit test-clock ownership"
)]

use super::*;
use futures_lite::future;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

fn fixture<P>() -> (LinkControl<P>, Arc<AtomicU64>) {
    let ticks = Arc::new(AtomicU64::new(1));
    let clock = ticks.clone();
    (
        LinkControl::new(move || LinkTime::from_nanos(clock.load(Ordering::Relaxed))).unwrap(),
        ticks,
    )
}

fn bind<P>(links: &mut LinkControl<P>, port: PortId, tag: u64) -> (BindingId, LinkTx<P>) {
    links
        .bind_send(Destination::Local {
            port,
            tag: DeliveryTag::from_raw(tag),
        })
        .unwrap()
}

#[test]
fn local_only_owned_values_keep_observation_and_bounded_fifo() {
    // Neither Clone nor a wire codec is required for local payloads.
    #[derive(Debug, PartialEq)]
    struct LocalOnly(u64);
    let (mut links, ticks) = fixture();
    let (port, mut rx) = links.create_port().unwrap();
    let (_, tx) = bind(&mut links, port, 12);
    let observed = ReceiveTime {
        monotonic: LinkTime::from_nanos(1),
        wall: Some(SystemTime::UNIX_EPOCH),
    };
    tx.try_send_received(LocalOnly(1), observed, LinkTime::from_nanos(10))
        .unwrap();
    tx.try_send(LocalOnly(2), LinkTime::from_nanos(10)).unwrap();
    ticks.store(5, Ordering::Relaxed);
    assert!(future::block_on(rx.readable()));
    let mut batch = Vec::new();
    assert_eq!(rx.try_recv_batch(&mut batch, 0), 0);
    assert_eq!(rx.try_recv_batch(&mut batch, 1), 1);
    assert_eq!(batch[0].packet, LocalOnly(1));
    assert_eq!(batch[0].tag.as_raw(), 12);
    assert_eq!(batch[0].received_at, observed);
    assert_eq!(rx.try_recv_batch(&mut batch, 1), 1);
    assert_eq!(batch[1].packet, LocalOnly(2));
}

#[test]
fn retired_binding_fences_clones_and_already_accepted_work_after_reuse() {
    let (mut links, _) = fixture();
    let (port, mut rx) = links.create_port().unwrap();
    let (binding, tx) = bind(&mut links, port, 1);
    let stale = tx.clone();
    tx.try_send(11, LinkTime::from_nanos(10)).unwrap();
    links.unbind(binding);
    let (replacement, fresh) = bind(&mut links, port, 2);
    assert_eq!(binding.slot, replacement.slot);
    links.unbind(binding); // Repeated retirement cannot retire the replacement.
    let error = stale.try_send(12, LinkTime::from_nanos(10)).unwrap_err();
    assert_eq!((error.kind, error.packet), (SendErrorKind::Stale, 12));
    fresh.try_send(13, LinkTime::from_nanos(10)).unwrap();
    let mut batch = Vec::new();
    assert_eq!(rx.try_recv_batch(&mut batch, 1), 0);
    assert_eq!(rx.drop_counts().stale, 1);
    assert_eq!(rx.try_recv_batch(&mut batch, 1), 1);
    assert_eq!((batch[0].tag.as_raw(), batch[0].packet), (2, 13));
}

#[test]
fn saturation_returns_ownership_and_expiry_does_not_escape_work_budget() {
    let (mut links, ticks) = fixture();
    let (port, mut rx) = links.create_port().unwrap();
    let (_, tx) = bind(&mut links, port, 1);
    for value in 0..QUEUE_CAPACITY.get() {
        tx.try_send(value, LinkTime::from_nanos(10)).unwrap();
    }
    let error = tx
        .try_send(usize::MAX, LinkTime::from_nanos(10))
        .unwrap_err();
    assert_eq!(
        (error.kind, error.packet),
        (SendErrorKind::Full, usize::MAX)
    );
    ticks.store(10, Ordering::Relaxed);
    let mut batch = Vec::new();
    assert_eq!(rx.try_recv_batch(&mut batch, 3), 0);
    assert_eq!(rx.drop_counts().expired, 3);
    tx.try_send(42, LinkTime::from_nanos(11)).unwrap();
    assert_eq!(rx.try_recv_batch(&mut batch, QUEUE_CAPACITY.get()), 1);
    assert_eq!(batch[0].packet, 42);
    assert_eq!(rx.drop_counts().expired, 1024);
}

#[test]
fn closed_port_wakes_parked_receiver_and_cannot_alias_replacement() {
    let (mut links, _) = fixture();
    let (port, mut rx) = links.create_port().unwrap();
    let (_, tx) = bind(&mut links, port, 1);
    future::block_on(async {
        let wait = rx.readable();
        futures_lite::pin!(wait);
        assert!(future::poll_once(wait.as_mut()).await.is_none());
        links.close_port(port);
        assert!(!wait.await);
    });
    let (replacement, mut new_rx) = links.create_port().unwrap();
    assert_eq!(port.slot, replacement.slot);
    links.close_port(port);
    let error = tx.try_send(1, LinkTime::from_nanos(10)).unwrap_err();
    assert_eq!(error.kind, SendErrorKind::Closed);
    let (_, fresh) = bind(&mut links, replacement, 2);
    fresh.try_send(2, LinkTime::from_nanos(10)).unwrap();
    let mut batch = Vec::new();
    assert_eq!(new_rx.try_recv_batch(&mut batch, 1), 1);
    assert_eq!(batch[0].packet, 2);
}

#[test]
fn control_drop_retires_queued_work_and_wakes_receiver_despite_live_clones() {
    let (mut links, _) = fixture();
    let (port, mut rx) = links.create_port().unwrap();
    let (_, tx) = bind(&mut links, port, 1);
    tx.try_send(1, LinkTime::from_nanos(10)).unwrap();
    drop(links);
    assert!(!future::block_on(rx.readable()));
    let mut batch = Vec::new();
    assert_eq!(rx.try_recv_batch(&mut batch, 1), 0);
    assert_eq!(rx.drop_counts().closed, 1);
    assert_eq!(
        tx.try_send(2, LinkTime::from_nanos(10)).unwrap_err().kind,
        SendErrorKind::Closed
    );
}

#[test]
fn independent_producers_transfer_owned_packets_without_retry() {
    let (mut links, _) = fixture();
    let (port, mut rx) = links.create_port().unwrap();
    let (_, tx) = bind(&mut links, port, 1);
    std::thread::scope(|scope| {
        for producer in 0..4 {
            let tx = tx.clone();
            scope.spawn(move || {
                for sequence in 0..64 {
                    tx.try_send((producer, sequence), LinkTime::from_nanos(10))
                        .unwrap();
                }
            });
        }
    });
    let mut batch = Vec::with_capacity(256);
    assert_eq!(rx.try_recv_batch(&mut batch, 256), 256);
    for producer in 0..4 {
        let sequences: Vec<_> = batch
            .iter()
            .filter(|item| item.packet.0 == producer)
            .map(|item| item.packet.1)
            .collect();
        assert_eq!(sequences, (0..64).collect::<Vec<_>>());
    }
}

#[test]
fn foreign_control_handles_are_rejected() {
    let (mut first, _) = fixture::<u8>();
    let (mut second, _) = fixture::<u8>();
    let (port, _) = first.create_port().unwrap();
    assert!(matches!(
        second.bind_send(Destination::Local {
            port,
            tag: DeliveryTag::from_raw(0)
        }),
        Err(BindError::Closed)
    ));
    let (other, mut rx) = second.create_port().unwrap();
    let (_, tx) = bind(&mut second, other, 1);
    second.close_port(port);
    first.close_port(other);
    tx.try_send(1, LinkTime::from_nanos(10)).unwrap();
    assert!(future::block_on(rx.readable()));
}
