#![allow(
    clippy::unwrap_used,
    reason = "the fixture uses known valid negotiated bindings and records"
)]

use core::num::{NonZeroU16, NonZeroU64};
use pulsebeam_proto::{
    data_control::{PublisherBinding, SubscriberBinding},
    data_endpoint::{Channel, DATA_V1, Format},
    data_record::MAX_RECORD_BYTES,
};

use super::*;

fn endpoints(delivery: Delivery) -> (Binding<u64>, Binding<u64>) {
    let source = NonZeroU64::MIN;
    let channel = Channel {
        association: 17,
        stream: NonZeroU16::MIN,
        incarnation: NonZeroU64::MIN,
    };
    let format = Format::negotiate(Some(DATA_V1), delivery).unwrap();
    (
        Binding {
            channel,
            format,
            endpoint: Endpoint::Publisher(PublisherBinding { source }),
        },
        Binding {
            channel,
            format,
            endpoint: Endpoint::Subscriber(SubscriberBinding {
                source,
                branch: NonZeroU64::MIN,
            }),
        },
    )
}

fn raw(unit: &OutgoingUnit, index: u16) -> Vec<u8> {
    let mut bytes = vec![0; MAX_RECORD_BYTES];
    let len = unit.write(index, &mut bytes).unwrap();
    bytes.truncate(len);
    bytes
}

#[test]
fn descriptor_capable_receiver_preserves_sparse_metadata_and_rejects_conflicting_retry() {
    let (tx, rx) = endpoints(Delivery::Unreliable);
    let mut publisher = Publisher::new(tx, None, 0).unwrap();
    let unit = publisher
        .admit(tx, vec![9; 1025], DataSendOptions::default())
        .unwrap();
    let first = raw(&unit, 0);
    let last = raw(&unit, 1);
    let descriptor = [0xc0, 0, 1];
    let mut scratch = [0; data_record::MAX_ENDPOINT_RECORD_BYTES];
    let extended =
        data_record::restore_endpoint_record(&last, Some(&descriptor), &mut scratch).unwrap();
    let mut legacy = Subscriber::new(rx, None).unwrap();
    let now = Duration::ZERO;
    assert!(matches!(
        legacy.offer(rx, extended, now),
        Err(Error::Record(_))
    ));
    let mut capable = Subscriber::new(rx, None).unwrap().with_record_dd(true);
    assert_eq!(capable.offer(rx, extended, now), Ok(None));
    assert_eq!(capable.offer(rx, extended, now), Ok(None));
    let assembled = capable.offer(rx, &first, now).unwrap().unwrap();
    assert_eq!(assembled.bytes, vec![9; 1025]);
    assert_eq!(
        assembled.fragment_descriptors,
        vec![DataFragmentDescriptor {
            fragment_index: 1,
            bytes: descriptor.to_vec(),
        }]
    );
    assert_eq!(capable.offer(rx, extended, now), Ok(None));

    let mut capable = Subscriber::new(rx, None).unwrap().with_record_dd(true);
    assert_eq!(capable.offer(rx, extended, now), Ok(None));
    assert_eq!(capable.offer(rx, &last, now), Err(Error::Conflict));
    assert_eq!(capable.offer(rx, &first, now), Ok(None));
}

#[test]
fn reliable_retry_reuses_the_initial_descriptor_bytes() {
    let (tx, _) = endpoints(Delivery::Reliable);
    let options = DataSendOptions {
        captured_at: None,
        fragment_descriptors: Some(vec![DataFragmentDescriptor {
            fragment_index: 0,
            bytes: vec![0xc0; data_record::MAX_DESCRIPTOR_BYTES],
        }]),
    };
    let now = Duration::ZERO;
    let mut legacy = ReliablePublisher::new(tx, None, 7).unwrap();
    assert_eq!(
        legacy.admit(tx, vec![0; 1024], options.clone(), now),
        Err(Error::Binding)
    );
    assert_eq!(
        legacy
            .admit(tx, vec![0; 1024], DataSendOptions::default(), now)
            .unwrap()
            .unit_seq,
        7
    );
    let mut capable = ReliablePublisher::new(tx, None, 7)
        .unwrap()
        .with_record_dd(true);
    let admitted = capable.admit(tx, vec![0; 1024], options, now).unwrap();
    let mut initial = vec![0; data_record::MAX_ENDPOINT_RECORD_BYTES];
    let len = capable
        .write_initial(admitted, 0, &mut initial, now)
        .unwrap();
    initial.truncate(len);
    assert_eq!(
        capable.next_action(CONTROL_RETRY),
        Ok(Some(AdmissionAction::RetryFirst {
            unit_seq: 7,
            record: initial
        }))
    );
}

#[test]
fn late_join_suffix_never_delivers_but_a_later_complete_unit_does() {
    let (tx, rx) = endpoints(Delivery::Unreliable);
    let mut publisher = Publisher::new(tx, None, 0).unwrap();
    let mut subscriber = Subscriber::new(rx, None).unwrap();
    let missed = publisher
        .admit(tx, vec![7; 1_025], DataSendOptions::default())
        .unwrap();
    let current = publisher
        .admit(tx, vec![9; 1_025], DataSendOptions::default())
        .unwrap();
    let now = Duration::ZERO;
    assert_eq!(subscriber.offer(rx, &raw(&missed, 1), now), Ok(None));
    assert_eq!(subscriber.offer(rx, &raw(&current, 1), now), Ok(None));
    assert_eq!(subscriber.offer(rx, &raw(&current, 1), now), Ok(None));
    assert_eq!(
        subscriber.offer(rx, &raw(&current, 0), now),
        Ok(Some(AssembledUnit {
            unit_seq: current.unit_seq(),
            bytes: vec![9; 1_025],
            captured_at: None,
            fragment_descriptors: vec![],
        }))
    );
    assert_eq!(subscriber.offer(rx, &raw(&current, 0), now), Ok(None));
    subscriber.expire(LIFETIME);
    assert_eq!(subscriber.offer(rx, &raw(&missed, 0), LIFETIME), Ok(None));
    assert_eq!(subscriber.offer(rx, &raw(&current, 1), LIFETIME), Ok(None));
    let replacement = Binding {
        channel: Channel {
            incarnation: rx.channel.incarnation.saturating_add(1),
            ..rx.channel
        },
        ..rx
    };
    let mut rejoined = Subscriber::new(replacement, None).unwrap();
    assert_eq!(
        rejoined.offer(rx, &raw(&current, 0), now),
        Err(Error::Binding)
    );
    assert_eq!(
        rejoined.offer(replacement, &raw(&current, 1), now),
        Ok(None)
    );
}

#[test]
fn latest_only_suppresses_an_older_unit_after_a_newer_delivery() {
    for mode in [Delivery::Unreliable, Delivery::Latest] {
        let (tx, rx) = endpoints(mode);
        let mut publisher = Publisher::new(tx, None, 10).unwrap();
        let mut subscriber = Subscriber::new(rx, None).unwrap();
        let mut make = || {
            publisher
                .admit(tx, vec![1; 1_025], DataSendOptions::default())
                .unwrap()
        };
        let oldest = make();
        let middle = make();
        let newest = make();
        let now = Duration::ZERO;
        for unit in [&oldest, &middle, &newest] {
            assert_eq!(subscriber.offer(rx, &raw(unit, 0), now), Ok(None));
        }
        assert_eq!(
            subscriber
                .offer(rx, &raw(&middle, 1), now)
                .unwrap()
                .unwrap()
                .unit_seq,
            11
        );
        assert_eq!(
            subscriber
                .offer(rx, &raw(&newest, 1), now)
                .unwrap()
                .unwrap()
                .unit_seq,
            12
        );
        let last = subscriber.offer(rx, &raw(&oldest, 1), now).unwrap();
        assert_eq!(
            last.map(|unit| unit.unit_seq),
            (mode == Delivery::Unreliable).then_some(10)
        );
    }
}

fn expectation(seq: u64, count: u32) -> data_control::Expectation {
    data_control::Expectation {
        unit: Some(data_control::BranchUnit {
            source: Some(data_control::SourceUnit {
                source_incarnation: 1,
                unit_seq: Some(seq),
            }),
            branch_incarnation: 1,
        }),
        fragment_count: count,
    }
}

#[test]
fn reliable_assembly_requires_the_bound_expectation_in_either_order() {
    let (tx, _) = endpoints(Delivery::Unreliable);
    let (_, rx) = endpoints(Delivery::Reliable);
    let mut publisher = Publisher::new(tx, None, 0).unwrap();
    let outgoing = publisher
        .admit(tx, vec![9; 1_025], DataSendOptions::default())
        .unwrap();
    let expected = expectation(outgoing.unit_seq(), 2);
    let now = Duration::ZERO;
    for expectation_first in [true, false] {
        let mut subscriber = Subscriber::new(rx, None).unwrap();
        let mut wrong_branch = expected;
        wrong_branch.unit.as_mut().unwrap().branch_incarnation = 2;
        assert_eq!(
            subscriber.expect(rx, wrong_branch, now),
            Err(Error::Control(data_control::Error::Binding))
        );
        assert_eq!(subscriber.missing_fragments(0, now), None);
        if expectation_first {
            assert_eq!(subscriber.expect(rx, expected, now), Ok(None));
            assert_eq!(subscriber.missing_fragments(0, now), Some(0b11));
        }
        assert_eq!(subscriber.offer(rx, &raw(&outgoing, 1), now), Ok(None));
        assert_eq!(
            subscriber.missing_fragments(0, now),
            expectation_first.then_some(0b01)
        );
        let demand = subscriber.next_repair(CONTROL_RETRY);
        if expectation_first {
            assert_eq!(
                demand.unwrap().record,
                Some(subscriber_control::Record::RequestRepair(
                    data_control::RepairDemand {
                        unit: expected.unit,
                        fragment_index: Some(0),
                        request_id: 1,
                    }
                ))
            );
        } else {
            assert_eq!(demand, None);
        }
        let last = subscriber
            .offer(rx, &raw(&outgoing, 0), CONTROL_RETRY)
            .unwrap();
        let assembled = if expectation_first {
            last
        } else {
            assert_eq!(last, None);
            subscriber.expect(rx, expected, now).unwrap()
        };
        assert_eq!(assembled.unwrap().bytes, vec![9; 1_025]);
        assert_eq!(subscriber.expect(rx, expected, now), Ok(None));
        assert_eq!(subscriber.offer(rx, &raw(&outgoing, 0), now), Ok(None));
        assert_eq!(subscriber.missing_fragments(0, now), None);
        assert_eq!(subscriber.repair_deadline(), None);
    }
}

#[test]
fn reliable_expectation_requests_exact_missing_fragments_without_live_ingress() {
    let (_, rx) = endpoints(Delivery::Reliable);
    let mut subscriber = Subscriber::new(rx, None).unwrap();
    let expected = expectation(3, 3);
    assert_eq!(subscriber.repair_deadline(), None);
    assert_eq!(subscriber.expect(rx, expected, Duration::ZERO), Ok(None));
    let before = CONTROL_RETRY - Duration::from_millis(1);
    assert_eq!(subscriber.expect(rx, expected, before), Ok(None));
    assert_eq!(subscriber.next_repair(before), None);
    assert_eq!(subscriber.repair_deadline(), Some(CONTROL_RETRY));
    for index in 0..3_u32 {
        let due = CONTROL_RETRY + REPAIR_SPACING * index;
        assert_eq!(subscriber.repair_deadline(), Some(due));
        let control = subscriber.next_repair(due).unwrap();
        assert_eq!(
            control.record,
            Some(subscriber_control::Record::RequestRepair(
                data_control::RepairDemand {
                    unit: expected.unit,
                    fragment_index: Some(index),
                    request_id: u64::from(index) + 1,
                }
            ))
        );
        assert_eq!(subscriber.next_repair(due), None);
    }
    assert_eq!(subscriber.repair_deadline(), None);
    assert_eq!(
        subscriber.expect(rx, expected, CONTROL_RETRY + REPAIR_SPACING * 3),
        Ok(None)
    );
    assert_eq!(subscriber.repair_deadline(), None);
    assert_eq!(subscriber.next_repair(LIFETIME), None);
}

#[test]
fn reliable_expectations_share_capacity_and_cannot_renew_expiry() {
    let (_, rx) = endpoints(Delivery::Reliable);
    let mut subscriber = Subscriber::new(rx, None).unwrap();
    for seq in 0..u64::try_from(UNIT_LIMIT).unwrap() {
        assert_eq!(
            subscriber.expect(rx, expectation(seq, 64), Duration::ZERO),
            Ok(None)
        );
    }
    let later = expectation(u64::try_from(UNIT_LIMIT).unwrap(), 1);
    assert_eq!(
        subscriber.expect(rx, later, Duration::ZERO),
        Err(Error::Capacity)
    );
    let first = expectation(0, 64);
    let replay = LIFETIME / 2;
    assert_eq!(subscriber.expect(rx, first, replay), Ok(None));
    assert_eq!(subscriber.missing_fragments(0, replay), Some(u64::MAX));
    assert_eq!(subscriber.next_expiry(), Some(LIFETIME));
    assert_eq!(subscriber.expect(rx, first, LIFETIME), Err(Error::Retired));
    assert_eq!(subscriber.missing_fragments(0, LIFETIME), None);
    assert_eq!(subscriber.expect(rx, later, LIFETIME), Ok(None));
}

#[test]
fn reliable_receipt_retries_only_after_assembly_and_stops_at_exact_ack_or_deadline() {
    let (tx, _) = endpoints(Delivery::Unreliable);
    let (_, rx) = endpoints(Delivery::Reliable);
    let mut publisher = Publisher::new(tx, None, 0).unwrap();
    let outgoing = publisher
        .admit(tx, vec![7], DataSendOptions::default())
        .unwrap();
    let expected = expectation(0, 1);
    let now = Duration::ZERO;
    for acknowledge in [false, true] {
        let mut subscriber = Subscriber::new(rx, None).unwrap();
        assert_eq!(subscriber.expect(rx, expected, now), Ok(None));
        assert_eq!(subscriber.next_receipt(now), None);
        assert_eq!(
            subscriber
                .offer(rx, &raw(&outgoing, 0), now)
                .unwrap()
                .unwrap()
                .bytes,
            vec![7]
        );
        assert_eq!(subscriber.receipt_ack(rx, expected, now), Ok(false));
        assert_eq!(subscriber.receipt_deadline(), Some(now));
        let send = ReceiptAction::Send(SubscriberControl {
            record: Some(subscriber_control::Record::AssemblyReceipt(expected)),
        });
        assert_eq!(subscriber.next_receipt(now), Some(send.clone()));
        assert_eq!(subscriber.next_receipt(now), None);
        assert_eq!(subscriber.receipt_deadline(), Some(CONTROL_RETRY));
        assert_eq!(subscriber.next_receipt(CONTROL_RETRY), Some(send));
        assert_eq!(
            subscriber.receipt_ack(rx, expectation(0, 2), CONTROL_RETRY),
            Err(Error::Conflict)
        );
        let mut wrong_branch = expected;
        wrong_branch.unit.as_mut().unwrap().branch_incarnation = 4;
        assert_eq!(
            subscriber.receipt_ack(rx, wrong_branch, CONTROL_RETRY),
            Err(Error::Control(data_control::Error::Binding))
        );
        assert_eq!(subscriber.expect(rx, expected, CONTROL_RETRY), Ok(None));
        if acknowledge {
            assert_eq!(
                subscriber.receipt_ack(rx, expected, CONTROL_RETRY),
                Ok(true)
            );
            assert_eq!(subscriber.next_receipt(LIFETIME), None);
        } else {
            assert_eq!(subscriber.receipt_ack(rx, expected, LIFETIME), Ok(false));
            assert_eq!(
                subscriber.next_receipt(LIFETIME),
                Some(ReceiptAction::Unconfirmed { unit_seq: 0 })
            );
        }
        assert_eq!(subscriber.receipt_deadline(), None);
        assert_eq!(subscriber.receipt_ack(rx, expected, LIFETIME), Ok(false));
        assert_eq!(subscriber.offer(rx, &raw(&outgoing, 0), LIFETIME), Ok(None));
    }
}

#[test]
fn pending_receipts_take_bounded_credit_even_after_unit_window_advances() {
    let (tx, _) = endpoints(Delivery::Unreliable);
    let (_, rx) = endpoints(Delivery::Reliable);
    let mut subscriber = Subscriber::new(rx, None).unwrap();
    let now = Duration::ZERO;
    for seq in (0..u64::try_from(UNIT_LIMIT).unwrap()).map(|index| index * WINDOW) {
        let mut publisher = Publisher::new(tx, None, seq).unwrap();
        let outgoing = publisher
            .admit(tx, vec![7], DataSendOptions::default())
            .unwrap();
        assert_eq!(subscriber.expect(rx, expectation(seq, 1), now), Ok(None));
        assert!(
            subscriber
                .offer(rx, &raw(&outgoing, 0), now)
                .unwrap()
                .is_some()
        );
    }
    let next = expectation(u64::try_from(UNIT_LIMIT).unwrap() * WINDOW, 1);
    assert_eq!(subscriber.expect(rx, next, now), Err(Error::Capacity));
    assert!(matches!(
        subscriber.next_receipt(now),
        Some(ReceiptAction::Send(_))
    ));
    assert_eq!(subscriber.receipt_ack(rx, expectation(0, 1), now), Ok(true));
    assert_eq!(subscriber.expect(rx, next, now), Ok(None));
    assert_eq!(
        subscriber.expect(rx, expectation(0, 1), now),
        Err(Error::Retired)
    );
}

#[test]
fn reliable_publisher_retries_identical_first_record_until_bound_admission() {
    let (tx, _) = endpoints(Delivery::Reliable);
    let now = Duration::ZERO;
    for acknowledged in [false, true] {
        let stamp = CaptureTimestamp {
            clock: CaptureClock {
                id: "camera".into(),
                epoch: u64::MAX,
            },
            nanos: i64::MIN,
            uncertainty_ns: Some(u32::MAX),
        };
        let mut publisher = ReliablePublisher::new(tx, Some(stamp.clock.clone()), 0).unwrap();
        let outgoing = publisher
            .admit(
                tx,
                vec![7; 1_025],
                DataSendOptions {
                    captured_at: Some(stamp),
                    fragment_descriptors: None,
                },
                now,
            )
            .unwrap();
        assert_eq!(publisher.next_action(now), Ok(None));
        assert_eq!(outgoing.fragment_count, 2);
        let mut original = vec![0; MAX_RECORD_BYTES];
        let len = publisher
            .write_initial(outgoing, 0, &mut original, now)
            .unwrap();
        original.truncate(len);
        let retry = || {
            Some(AdmissionAction::RetryFirst {
                unit_seq: 0,
                record: original.clone(),
            })
        };
        assert_eq!(publisher.deadline(), Some(CONTROL_RETRY));
        assert_eq!(publisher.next_action(CONTROL_RETRY), Ok(retry()));
        assert_eq!(publisher.next_action(CONTROL_RETRY), Ok(None));
        let ack = data_control::SourceUnit {
            source_incarnation: 1,
            unit_seq: Some(0),
        };
        assert_eq!(
            publisher.acknowledge(
                tx,
                data_control::SourceUnit {
                    source_incarnation: 2,
                    ..ack
                },
                CONTROL_RETRY
            ),
            Err(Error::Control(data_control::Error::Binding))
        );
        assert_eq!(
            publisher.next_action(CONTROL_RETRY.saturating_mul(2)),
            Ok(retry())
        );
        if acknowledged {
            assert_eq!(
                publisher.acknowledge(tx, ack, CONTROL_RETRY.saturating_mul(2)),
                Ok(true)
            );
            assert_eq!(
                publisher.acknowledge(tx, ack, CONTROL_RETRY.saturating_mul(2)),
                Ok(false)
            );
            assert_eq!(publisher.deadline(), Some(LIFETIME));
            assert_eq!(publisher.next_action(LIFETIME), Ok(None));
        } else {
            assert_eq!(publisher.acknowledge(tx, ack, LIFETIME), Ok(false));
            assert_eq!(
                publisher.next_action(LIFETIME),
                Ok(Some(AdmissionAction::Unconfirmed { unit_seq: 0 }))
            );
        }
        assert_eq!(publisher.deadline(), None);
        assert_eq!(publisher.acknowledge(tx, ack, LIFETIME), Ok(false));
        assert_eq!(
            publisher.write_initial(outgoing, 1, &mut original, LIFETIME),
            Err(Error::Retired)
        );
    }
}

#[test]
fn publisher_terminal_reports_are_per_branch_and_do_not_retain_payloads() {
    use data_control::{ControlOutcome, DeliveryOutcome};
    let (tx, _) = endpoints(Delivery::Reliable);
    let mut publisher = ReliablePublisher::new(tx, None, 0).unwrap();
    let admitted = publisher
        .admit(
            tx,
            vec![7; 1_025],
            DataSendOptions::default(),
            Duration::ZERO,
        )
        .unwrap();
    let expect = expectation(0, 2);
    let report = |expect, outcome: ControlOutcome| DeliveryOutcome {
        expectation: Some(expect),
        outcome: outcome as i32,
    };
    let confirmed = report(expect, ControlOutcome::Confirmed);

    assert_eq!(
        publisher.next_action(LIFETIME),
        Ok(Some(AdmissionAction::Unconfirmed { unit_seq: 0 }))
    );
    assert_eq!(
        publisher.write_initial(admitted, 0, &mut [0; MAX_RECORD_BYTES], LIFETIME),
        Err(Error::Retired)
    );
    let notice = PublisherDelivery {
        unit_seq: 0,
        branch: NonZeroU64::MIN,
        outcome: TerminalDelivery::Confirmed,
    };
    assert_eq!(
        publisher.observe_delivery(tx, confirmed, LIFETIME),
        Ok(Some(notice))
    );
    assert_eq!(
        publisher.observe_delivery(tx, confirmed, LIFETIME),
        Ok(None)
    );
    assert_eq!(
        publisher.observe_delivery(tx, report(expect, ControlOutcome::Unknown), LIFETIME),
        Err(Error::Conflict)
    );

    let mut other = expect;
    other.unit.as_mut().unwrap().branch_incarnation = 2;
    let notice = PublisherDelivery {
        unit_seq: 0,
        branch: NonZeroU64::new(2).unwrap(),
        outcome: TerminalDelivery::Failed,
    };
    assert_eq!(
        publisher.observe_delivery(tx, report(other, ControlOutcome::Failed), LIFETIME),
        Ok(Some(notice))
    );
    let wrong_count = expectation(0, 1);
    assert_eq!(
        publisher.observe_delivery(tx, report(wrong_count, ControlOutcome::Confirmed), LIFETIME),
        Err(Error::Conflict)
    );
    let mut wrong_source = expect;
    wrong_source
        .unit
        .as_mut()
        .unwrap()
        .source
        .as_mut()
        .unwrap()
        .source_incarnation = 2;
    assert_eq!(
        publisher.observe_delivery(
            tx,
            report(wrong_source, ControlOutcome::Confirmed),
            LIFETIME
        ),
        Err(Error::Control(data_control::Error::Binding))
    );
    assert_eq!(
        publisher.observe_delivery(tx, confirmed, LIFETIME.saturating_mul(3)),
        Ok(None)
    );
}

#[test]
fn publisher_original_lookup_is_exact_and_does_not_renew_the_retention_window() {
    let (tx, rx) = endpoints(Delivery::Reliable);
    let mut publisher = ReliablePublisher::new(tx, None, 0).unwrap();
    let admitted = publisher
        .admit(
            tx,
            vec![17; 1025],
            DataSendOptions::default(),
            Duration::ZERO,
        )
        .unwrap();
    let unit = data_control::SourceUnit {
        source_incarnation: 1,
        unit_seq: Some(admitted.unit_seq),
    };
    assert_eq!(publisher.acknowledge(tx, unit, Duration::ZERO), Ok(true));
    let request = |seq, source, index| data_control::OriginalDemand {
        unit: Some(data_control::SourceUnit {
            source_incarnation: source,
            unit_seq: Some(seq),
        }),
        fragment_index: Some(index),
        request_id: 1,
    };
    let mut original = [0; MAX_RECORD_BYTES];
    let expected = publisher
        .write_initial(admitted, 1, &mut original, Duration::ZERO)
        .unwrap();
    let mut result = [0; MAX_RECORD_BYTES];
    let before_expiry = LIFETIME.saturating_sub(Duration::from_millis(1));
    assert_eq!(
        publisher.write_original(rx, request(0, 1, 1), &mut result, before_expiry),
        Err(Error::Binding)
    );
    assert_eq!(
        publisher.write_original(tx, request(0, 2, 1), &mut result, before_expiry),
        Err(Error::Control(data_control::Error::Binding))
    );
    assert_eq!(
        publisher.write_original(tx, request(1, 1, 1), &mut result, before_expiry),
        Ok(None)
    );
    assert_eq!(
        publisher.write_original(tx, request(0, 1, 2), &mut result, before_expiry),
        Ok(None)
    );
    assert_eq!(
        publisher.write_original(tx, request(0, 1, 1), &mut result, before_expiry),
        Ok(Some(expected))
    );
    assert_eq!(&result[..expected], &original[..expected]);
    assert_eq!(publisher.deadline(), Some(LIFETIME));
    assert_eq!(
        publisher.write_original(tx, request(0, 1, 1), &mut result, LIFETIME),
        Ok(None)
    );
}

#[test]
fn reliable_publisher_retains_acknowledged_units_to_their_original_deadline() {
    let (tx, _) = endpoints(Delivery::Reliable);
    let mut publisher = ReliablePublisher::new(tx, None, 0).unwrap();
    for seq in 0..u64::try_from(UNIT_LIMIT).unwrap() {
        let unit = publisher
            .admit(tx, vec![7], DataSendOptions::default(), Duration::ZERO)
            .unwrap();
        assert_eq!(unit.unit_seq, seq);
        assert_eq!(
            publisher.acknowledge(
                tx,
                data_control::SourceUnit {
                    source_incarnation: 1,
                    unit_seq: Some(seq)
                },
                Duration::ZERO
            ),
            Ok(true)
        );
    }
    assert!(matches!(
        publisher.admit(tx, vec![], DataSendOptions::default(), Duration::ZERO),
        Err(Error::Capacity)
    ));
    assert_eq!(publisher.next_action(LIFETIME), Ok(None));
    let unit = publisher
        .admit(tx, vec![], DataSendOptions::default(), LIFETIME)
        .unwrap();
    assert_eq!(unit.unit_seq, u64::try_from(UNIT_LIMIT).unwrap());
}

#[test]
fn bounded_incomplete_units_expire_and_a_conflict_is_terminal() {
    let (tx, rx) = endpoints(Delivery::Unreliable);
    let mut publisher = Publisher::new(tx, None, 0).unwrap();
    let mut subscriber = Subscriber::new(rx, None).unwrap();
    let mut make = || {
        publisher
            .admit(tx, vec![5; 1_025], DataSendOptions::default())
            .unwrap()
    };
    let now = Duration::ZERO;
    let first = make();
    assert_eq!(subscriber.offer(rx, &raw(&first, 0), now), Ok(None));
    let mut changed = raw(&first, 0);
    *changed.last_mut().unwrap() = 6;
    assert_eq!(subscriber.offer(rx, &changed, now), Err(Error::Conflict));
    assert_eq!(subscriber.offer(rx, &raw(&first, 1), now), Ok(None));
    for _ in 0..UNIT_LIMIT {
        assert_eq!(subscriber.offer(rx, &raw(&make(), 0), now), Ok(None));
    }
    let overflow = make();
    assert_eq!(
        subscriber.offer(rx, &raw(&overflow, 0), now),
        Err(Error::Capacity)
    );
    assert_eq!(subscriber.next_expiry(), Some(LIFETIME));
    subscriber.expire(LIFETIME);
    assert_eq!(subscriber.next_expiry(), None);
    assert_eq!(subscriber.offer(rx, &raw(&overflow, 0), LIFETIME), Ok(None));
    assert!(
        subscriber
            .offer(rx, &raw(&overflow, 1), LIFETIME)
            .unwrap()
            .is_some()
    );
}

#[test]
fn capture_clock_and_sequence_exhaustion_fail_without_changing_unit_identity() {
    let (tx, rx) = endpoints(Delivery::Latest);
    let clock = CaptureClock {
        id: "capture".into(),
        epoch: u64::MAX,
    };
    let mut publisher = Publisher::new(tx, Some(clock.clone()), u64::MAX).unwrap();
    let mut subscriber = Subscriber::new(rx, Some(clock.clone())).unwrap();
    let missing = DataSendOptions {
        fragment_descriptors: None,
        captured_at: Some(CaptureTimestamp {
            clock: CaptureClock {
                epoch: 0,
                ..clock.clone()
            },
            nanos: 0,
            uncertainty_ns: None,
        }),
    };
    assert!(matches!(
        publisher.admit(tx, vec![], missing),
        Err(Error::Record(data_record::Error::Clock))
    ));
    let capture = CaptureTimestamp {
        clock,
        nanos: i64::MIN,
        uncertainty_ns: Some(u32::MAX),
    };
    let unit = publisher
        .admit(
            tx,
            vec![],
            DataSendOptions {
                captured_at: Some(capture.clone()),
                fragment_descriptors: None,
            },
        )
        .unwrap();
    assert_eq!(unit.count(), 1);
    assert_eq!(
        subscriber.offer(rx, &raw(&unit, 0), Duration::ZERO),
        Ok(Some(AssembledUnit {
            unit_seq: u64::MAX,
            captured_at: Some(capture),
            bytes: vec![],
            fragment_descriptors: vec![],
        }))
    );
    assert!(matches!(
        publisher.admit(tx, vec![], DataSendOptions::default()),
        Err(Error::Exhausted)
    ));
}
