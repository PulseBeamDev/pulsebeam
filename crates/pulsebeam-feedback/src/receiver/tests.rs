use super::*;
use crate::allocation::allocations;

fn config(order: ReportOrder) -> ReceiverConfig {
    ReceiverConfig {
        capacity: 8,
        max_report_statuses: 3,
        interval: Duration::from_millis(7),
        order,
    }
}

fn receipt(space: u64, packet: u64, millis: u64) -> Receipt {
    Receipt {
        space: SpaceId(space),
        packet: PacketId(packet),
        arrived_at: Duration::from_millis(millis),
        ecn: Some(Ecn::Ce),
        tag: 123,
    }
}

#[test]
fn normalized_continuation_late_holes_backpressure_and_epoch_fencing() {
    let mut receiver =
        Receiver::new(config(ReportOrder::Continuous(SpaceId(42))), Epoch(9)).unwrap();
    receiver.reserve(usize::MAX).unwrap();
    receiver.observe(Epoch(9), receipt(42, 100, 0)).unwrap();
    receiver.observe(Epoch(9), receipt(42, 103, 2)).unwrap();
    assert!(receiver.report(Duration::from_millis(6)).is_none());
    let now = Duration::from_millis(7);
    let report = receiver.report(now).unwrap();
    assert_eq!((report.base, report.count), (PacketId(100), 3));
    assert_eq!(receiver.first_receipt(report), Some(&receipt(42, 100, 0)));
    assert_eq!(receiver.receipt(SpaceId(42), PacketId(101)), None);
    assert_eq!(receiver.accept(report, 0, now), Err(Error::InvalidRange));
    receiver.accept(report, 2, now).unwrap();
    assert_eq!(receiver.deadline(), Some(now));
    assert_eq!(receiver.accept(report, 1, now), Err(Error::StaleReport));
    let continuation = receiver.report(now).unwrap();
    assert_eq!((continuation.base, continuation.count), (PacketId(102), 2));
    assert_eq!(
        receiver.first_receipt(continuation),
        Some(&receipt(42, 103, 2))
    );
    receiver.defer(now, Duration::from_millis(10)).unwrap();
    assert_eq!(receiver.defer(now, now), Err(Error::InvalidRange));
    assert!(receiver.report(now).is_none());
    assert_eq!(
        receiver.accept(continuation, 1, Duration::from_millis(10)),
        Err(Error::StaleReport)
    );
    let report = receiver.report(Duration::from_millis(10)).unwrap();
    receiver
        .accept(report, report.count, Duration::from_millis(10))
        .unwrap();
    assert_eq!(receiver.deadline(), None);
    receiver.observe(Epoch(9), receipt(42, 101, 11)).unwrap();
    let late = receiver.report(Duration::from_millis(18)).unwrap();
    assert_eq!((late.base, late.count), (PacketId(101), 3));
    let mut duplicate = receipt(42, 101, 12);
    duplicate.ecn = None;
    assert_eq!(
        receiver.observe(Epoch(9), duplicate),
        Ok(Observation::Duplicate)
    );
    assert_eq!(
        receiver.receipt(SpaceId(42), PacketId(101)),
        Some(&receipt(42, 101, 11))
    );
    assert_eq!(
        receiver.observe(Epoch(8), receipt(42, 104, 12)),
        Err(Error::WrongEpoch)
    );
    assert_eq!(
        receiver.observe(Epoch(9), receipt(43, 104, 12)),
        Err(Error::WrongSpace)
    );
    assert_eq!(
        receiver.observe(Epoch(9), receipt(42, u64::MAX, 12)),
        Err(Error::InvalidRange)
    );
    let bytes = receiver.storage_bytes();
    receiver.clear(Epoch(10));
    assert_eq!(receiver.storage_bytes(), bytes);
    assert!(receiver.is_empty());
    assert_eq!(
        receiver.accept(late, 1, Duration::from_millis(18)),
        Err(Error::StaleReport)
    );
    assert_eq!(
        receiver.observe(Epoch(9), receipt(42, 101, 19)),
        Err(Error::WrongEpoch)
    );
    receiver.observe(Epoch(10), receipt(42, 101, 19)).unwrap();
    assert_eq!(
        receiver.receipt(SpaceId(42), PacketId(101)),
        Some(&receipt(42, 101, 19))
    );
}

#[test]
fn independent_spaces_preserve_report_order_and_explicit_retirement() {
    let mut config = config(ReportOrder::Independent);
    config.capacity = 4;
    config.max_report_statuses = 2;
    let mut receiver = Receiver::new(config, Epoch(3)).unwrap();
    receiver.reserve(usize::MAX).unwrap();
    for (space, packet) in [(7, 70_000), (8, 70_000), (7, 70_002), (9, 2)] {
        assert_eq!(
            receiver.observe(Epoch(3), receipt(space, packet, 0)),
            Ok(Observation::Inserted { retired: 0 })
        );
    }
    let now = Duration::from_millis(7);
    let first = receiver.report(now).unwrap();
    assert_eq!(
        (first.space, first.base, first.count),
        (SpaceId(7), PacketId(70_000), 2)
    );
    receiver.accept(first, 1, now).unwrap();
    let next = receiver.report(now).unwrap();
    assert_eq!(
        (next.space, next.base, next.count),
        (SpaceId(8), PacketId(70_000), 1)
    );
    receiver.accept(next, next.count, now).unwrap();
    let next = receiver.report(now).unwrap();
    assert_eq!(
        (next.space, next.base, next.count),
        (SpaceId(7), PacketId(70_001), 2)
    );
    receiver.accept(next, next.count, now).unwrap();
    let next = receiver.report(now).unwrap();
    assert_eq!(
        (next.space, next.base, next.count),
        (SpaceId(9), PacketId(2), 1)
    );
    receiver.accept(next, next.count, now).unwrap();
    assert_eq!(receiver.deadline(), None);
    assert_eq!(
        receiver.observe(Epoch(3), receipt(8, 70_001, 8)),
        Ok(Observation::Inserted { retired: 1 })
    );
    assert_eq!(receiver.len(), 4);
    assert_eq!(receiver.receipt(SpaceId(7), PacketId(70_000)), None);
    assert_eq!(receiver.highest(SpaceId(8)), Some(PacketId(70_001)));
}

#[test]
fn funding_bounds_hot_path_and_clear_abort_lifetimes() {
    let config = config(ReportOrder::Continuous(SpaceId(1)));
    let required = config.minimum_storage_bytes().unwrap();
    let mut receiver = Receiver::new(config, Epoch(0)).unwrap();
    assert_eq!(
        receiver.reserve(required.saturating_sub(1)),
        Err(Error::Exhausted)
    );
    assert_eq!(receiver.storage_bytes(), 0);
    assert_eq!(
        receiver.observe(Epoch(0), receipt(1, 1, 0)),
        Err(Error::Exhausted)
    );
    receiver.reserve(required).unwrap();
    assert_eq!(receiver.storage_bytes(), required);
    assert_eq!(
        allocations(|| {
            for packet in 0..1000 {
                receiver
                    .observe(Epoch(0), receipt(1, packet, packet))
                    .unwrap();
            }
        }),
        0
    );
    assert_eq!(receiver.len(), config.capacity);
    assert_eq!(receiver.storage_bytes(), required);
    assert_eq!(
        receiver.observe(Epoch(0), receipt(1, 0, 1001)),
        Ok(Observation::TooOld)
    );
    assert_eq!(
        receiver.reserve(required.saturating_sub(1)),
        Err(Error::Exhausted)
    );
    assert_eq!(receiver.deadline(), Some(Duration::from_millis(7)));
    let mut count = 0;
    while let Some(report) = receiver.report(Duration::from_secs(2)) {
        count += report.count;
        receiver
            .accept(report, report.count, Duration::from_secs(2))
            .unwrap();
    }
    assert_eq!(count, config.capacity);
    receiver.clear(Epoch(1));
    assert_eq!(receiver.storage_bytes(), required);
    receiver.abort();
    assert_eq!(receiver.storage_bytes(), 0);
    assert_eq!(receiver.deadline(), None);
}

#[test]
fn validates_extreme_configuration_and_packet_bounds() {
    let config = config(ReportOrder::Independent);
    for invalid in [
        ReceiverConfig {
            capacity: 0,
            ..config
        },
        ReceiverConfig {
            max_report_statuses: 0,
            ..config
        },
        ReceiverConfig {
            interval: Duration::ZERO,
            ..config
        },
        ReceiverConfig {
            capacity: usize::MAX,
            ..config
        },
    ] {
        assert!(matches!(
            Receiver::new(invalid, Epoch(0)),
            Err(Error::InvalidConfig)
        ));
    }
    let mut receiver = Receiver::new(config, Epoch(0)).unwrap();
    receiver.reserve(usize::MAX).unwrap();
    receiver
        .observe(Epoch(0), receipt(1, u64::MAX - 1, 0))
        .unwrap();
    let report = receiver.report(Duration::from_millis(7)).unwrap();
    assert_eq!((report.base, report.count), (PacketId(u64::MAX - 1), 1));
    receiver
        .accept(report, 1, Duration::from_millis(7))
        .unwrap();
    assert_eq!(receiver.deadline(), None);
}
