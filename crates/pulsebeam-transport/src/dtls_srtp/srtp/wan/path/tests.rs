use super::*;

const PROBE_PREFIX: [u8; 13] = [3, 2, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0x04, 0xc0];
const RECEIPT: [u8; 13] = [3, 2, 2, 0, 0, 0, 0, 0, 0, 0, 1, 0x04, 0xc0];

fn binding() -> Binding {
    Binding {
        epoch: NonZeroU64::MIN,
        generation: NonZeroU64::MIN,
        peer: "[::1]:12345".parse().unwrap(),
    }
}

fn at(t: Instant, seconds: u64) -> Instant {
    t.checked_add(Duration::from_secs(seconds)).unwrap()
}

#[test]
fn exact_probe_and_receipt_qualify_one_direction_after_full_handoff() {
    let now = Instant::now();
    let key = binding();
    let mut sender = Path::after_accept(key, Role::Client, now);
    let mut receiver = Path::after_accept(key, Role::Server, now);
    assert_eq!(sender.lease(now), None);
    assert_eq!(receiver.lease(now), None);
    assert_eq!(receiver.begin_probe(now), None);
    let probe = sender.begin_probe(now).unwrap();
    let image = probe.encode();
    assert_eq!(image[..13], PROBE_PREFIX);
    assert!(image[13..].iter().all(|byte| *byte == 0));
    assert_eq!(image.len(), PROBE_BYTES);
    assert_eq!(sender.begin_probe(now), None);
    assert!(!sender.probe_submitted(now, probe, MAX_DATAGRAM_BYTES - 1));
    assert_eq!(sender.receive(now, key, &RECEIPT, 35), None);
    assert_eq!(sender.lease(now), None);
    assert!(sender.probe_submitted(now, probe, MAX_DATAGRAM_BYTES));
    assert!(!sender.probe_submitted(now, probe, MAX_DATAGRAM_BYTES));
    assert_eq!(
        receiver.receive(now, key, &image, MAX_DATAGRAM_BYTES - 1),
        None
    );
    assert_eq!(receiver.begin_probe(now), None);
    let mut bad = image;
    bad[13] = 1;
    assert_eq!(receiver.receive(now, key, &bad, MAX_DATAGRAM_BYTES), None);
    let receipt = receiver
        .receive(now, key, &image, MAX_DATAGRAM_BYTES)
        .unwrap();
    assert_eq!(receipt.encode(), RECEIPT);
    assert_eq!(receiver.receive(now, key, &image, MAX_DATAGRAM_BYTES), None);
    assert!(receiver.begin_probe(now).is_some());
    let mut bad = RECEIPT;
    bad[10] = 0;
    assert_eq!(sender.receive(now, key, &bad, 35), None);
    assert_eq!(sender.receive(now, key, &RECEIPT, 36), None);
    assert_eq!(
        sender
            .receive(now, key, &image, MAX_DATAGRAM_BYTES)
            .unwrap()
            .encode(),
        RECEIPT
    );
    assert_eq!(sender.lease(now), None);
    assert_eq!(sender.receive(now, key, &RECEIPT, 35), None);
    assert!(sender.lease(now).is_some());
    assert_eq!(receiver.lease(now), None);
}

#[test]
fn lost_revalidation_and_expiry_fence_queued_work_until_fresh_evidence() {
    let start = Instant::now();
    let key = binding();
    let mut path = Path::after_accept(key, Role::Client, start);
    let mut peer = Path::after_accept(key, Role::Server, start);
    let probe = path.begin_probe(start).unwrap();
    path.probe_submitted(start, probe, MAX_DATAGRAM_BYTES);
    let receipt = peer
        .receive(start, key, &probe.encode(), MAX_DATAGRAM_BYTES)
        .unwrap()
        .encode();
    path.receive(start, key, &receipt, 35);
    let initial_lease = path.lease(start).unwrap();
    assert_eq!(path.qualification_expires_at(), Some(at(start, 6)));
    assert_eq!(path.next_probe_at(), Some(at(start, 2)));
    assert_eq!(path.begin_probe(at(start, 1)), None);
    let lost = path.begin_probe(at(start, 2)).unwrap();
    assert_ne!(lost, probe);
    path.probe_submitted(at(start, 2), lost, MAX_DATAGRAM_BYTES);
    let lost_receipt = peer
        .receive(at(start, 2), key, &lost.encode(), MAX_DATAGRAM_BYTES)
        .unwrap()
        .encode();
    assert_eq!(path.lease(at(start, 3)), Some(initial_lease));
    assert_eq!(path.receive(at(start, 4), key, &lost_receipt, 35), None);
    assert_eq!(path.qualification_expires_at(), Some(at(start, 6)));
    assert_eq!(path.lease(at(start, 6)), None);

    let fresh = path.begin_probe(at(start, 6)).unwrap();
    path.probe_submitted(at(start, 6), fresh, MAX_DATAGRAM_BYTES);
    path.receive(at(start, 6), key, &receipt, 35);
    assert_eq!(path.lease(at(start, 6)), None);
    let receipt = peer
        .receive(at(start, 6), key, &fresh.encode(), MAX_DATAGRAM_BYTES)
        .unwrap()
        .encode();
    path.receive(at(start, 6), key, &receipt, 35);
    let lease = path.lease(at(start, 6)).unwrap();
    assert_ne!(lease, initial_lease);
    let renewal = path.begin_probe(at(start, 8)).unwrap();
    path.probe_submitted(at(start, 8), renewal, MAX_DATAGRAM_BYTES);
    let receipt = peer
        .receive(at(start, 8), key, &renewal.encode(), MAX_DATAGRAM_BYTES)
        .unwrap()
        .encode();
    path.receive(at(start, 8), key, &receipt, 35);
    assert_eq!(path.lease(at(start, 12)), Some(lease));
    assert_eq!(path.lease(at(start, 14)), None);
}

#[test]
fn replacement_and_mtu_failure_invalidate_old_inflight_receipts() {
    let start = Instant::now();
    let key = binding();
    let mut path = Path::after_accept(key, Role::Client, start);
    let probe = path.begin_probe(start).unwrap();
    path.probe_submitted(start, probe, MAX_DATAGRAM_BYTES);
    let other_epoch = Binding {
        epoch: key.epoch.saturating_add(1),
        ..key
    };
    assert!(!path.replace(start, other_epoch));
    path.receive(start, other_epoch, &RECEIPT, 35);
    assert_eq!(path.lease(start), None);
    path.receive(start, key, &RECEIPT, 35);
    assert!(path.lease(start).is_some());
    let inflight = path.begin_probe(at(start, 2)).unwrap();
    path.probe_submitted(at(start, 2), inflight, MAX_DATAGRAM_BYTES);
    let mut second_receipt = RECEIPT;
    second_receipt[10] = 2;
    path.fail_mtu(at(start, 2), key);
    path.receive(at(start, 2), key, &second_receipt, 35);
    assert_eq!(path.lease(at(start, 2)), None);
    let moved = Binding {
        generation: key.generation.saturating_add(1),
        ..key
    };
    assert!(path.replace(at(start, 2), moved));
    assert!(!path.replace(at(start, 2), key));
    assert_eq!(path.begin_probe(at(start, 3)), None);
    let next = path.begin_probe(at(start, 4)).unwrap();
    assert_eq!(next.encode()[10], 3);
    assert!(!path.probe_submitted(at(start, 4), inflight, MAX_DATAGRAM_BYTES));
    assert!(path.probe_submitted(at(start, 4), next, MAX_DATAGRAM_BYTES));
    let mut third_receipt = RECEIPT;
    third_receipt[10] = 3;
    path.receive(at(start, 4), key, &third_receipt, 35);
    assert_eq!(path.lease(at(start, 4)), None);
    path.receive(at(start, 4), moved, &third_receipt, 35);
    assert!(path.lease(at(start, 4)).is_some());
    assert_eq!(path.lease(at(start, 3)), None);
}
