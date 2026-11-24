use super::*;

#[allow(
    clippy::disallowed_methods,
    reason = "one clock origin; all test time advances are explicit"
)]
fn origin() -> Instant {
    Instant::now()
}

type TestBuffer = Buffer<u8, u32>;

fn packet(source: u8, sequence: u32, timestamp: u32, at: Instant, bytes: usize) -> Input<u8, u32> {
    Input {
        source,
        sequence,
        timestamp,
        arrived_at: at,
        retained_bytes: NonZeroUsize::new(bytes).unwrap(),
        packet: sequence,
    }
}

fn time(t0: Instant, ms: u64) -> Instant {
    t0.checked_add(Duration::from_millis(ms)).unwrap()
}

fn limits(reorder: u32, packets: usize, bytes: usize) -> Limits {
    Limits::new(Duration::from_millis(10), reorder, packets, bytes).unwrap()
}

fn queued(buffer: &mut TestBuffer, at: Instant, sequence: u32) {
    assert!(matches!(
        buffer.push(at, packet(1, sequence, sequence, at, 4)),
        Push::Queued
    ));
}

fn take(buffer: &mut TestBuffer, at: Instant) -> (u64, Option<u64>) {
    match buffer.poll(at).unwrap().unwrap() {
        Output::Packet {
            sequence,
            timestamp,
            input,
        } => {
            assert_eq!(input.packet, input.sequence);
            (sequence, timestamp)
        }
        Output::Gap { .. } => panic!("expected ordered packet"),
    }
}

#[test]
fn reorders_before_deadline_and_drops_duplicates_without_restarting_wait() {
    let t0 = origin();
    let mut b = Buffer::new(1, limits(64, 4, 32));
    queued(&mut b, t0, 100);
    assert_eq!(b.deadline(), Some(t0));
    assert_eq!(take(&mut b, t0), (100, Some(100)));
    assert_eq!(b.deadline(), None);
    let t1 = time(t0, 1);
    queued(&mut b, t1, 102);
    assert_eq!(b.deadline(), Some(time(t0, 11)));
    assert!(matches!(
        b.push(time(t0, 2), packet(1, 102, 200, time(t0, 2), 4)),
        Push::Rejected {
            reason: Rejected::Duplicate,
            ..
        }
    ));
    assert_eq!(b.deadline(), Some(time(t0, 11)));
    assert!(b.poll(time(t0, 8)).unwrap().is_none());
    let t9 = time(t0, 9);
    queued(&mut b, t9, 101);
    assert_eq!(take(&mut b, t9), (101, Some(101)));
    assert_eq!(take(&mut b, t9), (102, Some(102)));
    assert_eq!(b.usage(), Usage::default());
    assert_eq!(b.deadline(), None);
}

#[test]
fn clock_alone_settles_gaps_and_late_input_cannot_reopen_them() {
    let t0 = origin();
    let mut b = Buffer::new(1, limits(64, 4, 32));
    queued(&mut b, t0, 100);
    take(&mut b, t0);
    queued(&mut b, time(t0, 1), 104);
    queued(&mut b, time(t0, 9), 102);
    let expiry = time(t0, 11);
    // A closer packet cannot keep an older packet waiting past its deadline.
    assert_eq!(b.deadline(), Some(expiry));
    assert!(matches!(
        b.push(expiry, packet(1, 101, 101, expiry, 4)),
        Push::Rejected {
            reason: Rejected::Late,
            ..
        }
    ));
    assert!(
        matches!(b.poll(expiry).unwrap(), Some(Output::Gap { sequence, reason: GapReason::Deadline }) if sequence == (101..102))
    );
    assert_eq!(take(&mut b, expiry).0, 102);
    assert!(
        matches!(b.poll(expiry).unwrap(), Some(Output::Gap { sequence, reason: GapReason::Deadline }) if sequence == (103..104))
    );
    assert_eq!(take(&mut b, expiry).0, 104);
    assert!(matches!(
        b.push(expiry, packet(1, 103, 103, expiry, 4)),
        Push::Rejected {
            reason: Rejected::Late,
            ..
        }
    ));
    assert!(b.poll(time(t0, 1000)).unwrap().is_none());
    assert_eq!(b.deadline(), None);
    assert!(matches!(b.poll(expiry), Err(ClockWentBackwards)));
}

#[test]
fn wrap_keeps_source_order_without_forcing_timestamp_order() {
    let now = origin();
    let mut b = Buffer::new(1, limits(64, 4, 32));
    let base = u64::from(u32::MAX);
    assert!(matches!(
        b.push(now, packet(1, u32::MAX, u32::MAX, now, 4)),
        Push::Queued
    ));
    assert_eq!(take(&mut b, now), (base, Some(base)));
    assert!(matches!(b.push(now, packet(1, 1, 0, now, 4)), Push::Queued));
    assert!(matches!(b.push(now, packet(1, 0, 2, now, 4)), Push::Queued));
    assert_eq!(
        take(&mut b, now),
        (1 << 32, Some((1_u64 << 32).saturating_add(2)))
    );
    assert_eq!(
        take(&mut b, now),
        ((1_u64 << 32).saturating_add(1), Some(1 << 32))
    );
    assert!(matches!(
        b.push(now, packet(1, 2, 0x8000_0002, now, 4)),
        Push::Queued
    ));
    assert_eq!(take(&mut b, now).1, None);
    assert!(matches!(
        b.push(now, packet(1, 0x8000_0003, 1, now, 4)),
        Push::Rejected {
            reason: Rejected::Sequence(ExtensionError::Ambiguous),
            ..
        }
    ));
}

#[test]
fn capacity_and_outer_budget_allow_at_most_one_nearer_replacement() {
    let now = origin();
    let mut b = Buffer::new(1, limits(64, 2, 10));
    queued(&mut b, now, 200);
    take(&mut b, now);
    queued(&mut b, now, 202);
    assert!(matches!(
        b.push(now, packet(1, 204, 204, now, 6)),
        Push::Queued
    ));
    assert_eq!(
        b.usage(),
        Usage {
            packets: 2,
            bytes: 10
        }
    );
    assert!(matches!(
        b.push_with_headroom(now, packet(1, 201, 201, now, 7), Usage::default()),
        Push::Rejected {
            reason: Rejected::Capacity,
            ..
        }
    ));
    assert!(
        matches!(b.push_with_headroom(now, packet(1, 201, 201, now, 5), Usage::default()),
            Push::Evicted(input) if input.sequence == 204
        )
    );
    assert_eq!(
        b.usage(),
        Usage {
            packets: 2,
            bytes: 9
        }
    );
    assert_eq!(take(&mut b, now).0, 201);
    assert_eq!(take(&mut b, now).0, 202);
    assert!(matches!(
        b.push_with_headroom(
            now,
            packet(1, 203, 203, now, 4),
            Usage {
                packets: 0,
                bytes: 10
            }
        ),
        Push::Rejected {
            reason: Rejected::Capacity,
            ..
        }
    ));
    assert_eq!(b.usage(), Usage::default());
}

#[test]
fn distant_gaps_take_one_event_and_source_replacement_fences_old_input() {
    let now = origin();
    let mut b = Buffer::new(1, limits(3, 4, 32));
    queued(&mut b, now, 200);
    take(&mut b, now);
    queued(&mut b, now, 250);
    assert_eq!(b.deadline(), Some(now));
    assert!(
        matches!(b.poll(now).unwrap(), Some(Output::Gap { sequence, reason: GapReason::ReorderLimit }) if sequence == (201..250))
    );
    assert_eq!(b.replace_source(1), Usage::default());
    assert_eq!(
        b.replace_source(2),
        Usage {
            packets: 1,
            bytes: 4
        }
    );
    assert_eq!(b.deadline(), None);
    assert!(matches!(
        b.push(now, packet(1, 250, 250, now, 4)),
        Push::Rejected {
            reason: Rejected::Retired,
            ..
        }
    ));
    assert!(matches!(
        b.push(now, packet(2, 4, 99, now, 4)),
        Push::Queued
    ));
    assert_eq!(take(&mut b, now), (4, Some(99)));
}
