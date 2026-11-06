use super::*;
use crate::allocation::allocations;

mod adversarial;

const EPOCH: Epoch = Epoch(70);
const SPACE: SpaceId = SpaceId(1_u64 << 40);
const OTHER: SpaceId = SpaceId((1_u64 << 40) + 1);
const RECEIVED: SendStatus = SendStatus {
    received: true,
    arrival: Some(ReceiverTime::Micros(-17)),
    ecn: Some(EcnMark::Ce),
    hold: Some(Duration::from_millis(2)),
};
const MISSING: SendStatus = SendStatus {
    received: false,
    arrival: None,
    ecn: None,
    hold: None,
};

fn config() -> SenderConfig {
    SenderConfig {
        capacity: 7,
        retention: Duration::from_millis(100),
        max_expirations_per_poll: 4,
        max_ack_reordering: 20,
        max_feedback_statuses: 7,
        initial_reordering_window: Duration::from_millis(7),
        max_reordering_window: Duration::from_millis(15),
    }
}

fn sender(mode: FeedbackMode) -> Sender {
    let mut sender = Sender::new(config(), mode, 64 * 1024).unwrap();
    sender.set_availability(EPOCH, true);
    sender.clear_observations();
    sender
}

fn context(mode: FeedbackMode, space: SpaceId, packet: u64) -> SendCommit {
    SendCommit {
        at: Duration::ZERO,
        wire_len: 123,
        path_epoch: EPOCH,
        identity: SendIdentity {
            space,
            packet: PacketId(packet),
            feedback_sequence: (mode == FeedbackMode::Continuous).then_some(PacketId(packet)),
            tag: u64::MAX - packet,
        },
    }
}

fn send(sender: &mut Sender, context: SendCommit) {
    sender.prepare_commit(context).unwrap();
    sender.commit_preflighted(context).unwrap();
    sender.assert_storage();
}

fn feedback(
    sender: &mut Sender,
    mode: FeedbackMode,
    space: SpaceId,
    base: u64,
    generation: i64,
    at: Duration,
    statuses: &[SendStatus],
) {
    let reports = [IndependentReport {
        space,
        base: PacketId(base),
        generation,
        statuses,
    }];
    sender.process_feedback(SendFeedback {
        received_at: at,
        path_epoch: EPOCH,
        report: if mode == FeedbackMode::Continuous {
            SendReport::Continuous {
                base_sequence: PacketId(base),
                reference_micros: generation * 10_000,
                generation,
                statuses,
            }
        } else {
            SendReport::Independent { reports: &reports }
        },
    });
    sender.assert_storage();
}

#[test]
fn independent_spaces_and_span_credit_keep_receipt_and_loss_distinct() {
    for mode in [FeedbackMode::Continuous, FeedbackMode::Independent] {
        let mut sender = sender(mode);
        send(&mut sender, context(mode, SPACE, 10));
        send(&mut sender, context(mode, SPACE, 11));
        feedback(
            &mut sender,
            mode,
            SPACE,
            10,
            1,
            Duration::from_millis(10),
            &[MISSING, RECEIVED],
        );
        let inputs = sender.observations();
        assert_eq!(inputs.bytes_in_flight, 0);
        assert_eq!(
            inputs
                .feedback
                .iter()
                .filter(|item| item.newly_acked)
                .count(),
            2
        );
        assert_eq!(
            inputs.feedback.iter().filter(|item| item.received).count(),
            1
        );
        assert_eq!(
            inputs
                .feedback
                .iter()
                .any(|item| item.reported_lost_for_the_first_time),
            mode == FeedbackMode::Independent
        );
        assert!(inputs.fresh_network_feedback);
        assert_eq!(
            inputs
                .feedback
                .iter()
                .find(|item| item.received)
                .unwrap()
                .receiver_arrival,
            RECEIVED.arrival
        );
        assert_eq!(
            inputs
                .feedback
                .iter()
                .find(|item| item.received)
                .unwrap()
                .ecn,
            RECEIVED.ecn
        );
        assert_eq!(
            inputs.timing.unwrap().feedback_hold,
            Some(Duration::from_millis(2))
        );
        assert_eq!(
            sender.next_deadline(Duration::from_millis(10)),
            Some(Duration::from_millis(17))
        );
        sender.clear_observations();
        assert!(sender.expire(Duration::from_millis(20), 4) <= 4);
        assert_eq!(sender.counters().not_received, 1);
        assert!(
            sender
                .observations()
                .synthetic
                .iter()
                .any(|item| item.loss_origin == Some(LossOrigin::Reordering))
        );
        sender.clear_observations();
        feedback(
            &mut sender,
            mode,
            SPACE,
            10,
            2,
            Duration::from_millis(21),
            &[RECEIVED],
        );
        let recovered = sender.observations().feedback.last().unwrap();
        assert!(recovered.received);
        assert!(!recovered.newly_acked);
        assert_eq!(recovered.recovered, mode == FeedbackMode::Independent);
        assert_eq!(recovered.tag, u64::MAX - 10);
        assert_eq!(sender.counters().not_received, 1);
        assert_eq!(sender.counters().received, 1);
        sender.clear_observations();
        feedback(
            &mut sender,
            mode,
            SPACE,
            10,
            2,
            Duration::from_millis(22),
            &[RECEIVED],
        );
        assert!(!sender.observations().fresh_network_feedback);
        assert!(sender.observations().feedback.is_empty());
        assert_eq!(sender.observations().bytes_in_flight, 0);
    }

    let mode = FeedbackMode::Independent;
    let mut sender = sender(mode);
    send(&mut sender, context(mode, SPACE, 10));
    send(&mut sender, context(mode, OTHER, 10));
    feedback(
        &mut sender,
        mode,
        SPACE,
        10,
        1,
        Duration::from_millis(10),
        &[RECEIVED],
    );
    assert_eq!(sender.observations().bytes_in_flight, 123);
    sender.clear_observations();
    feedback(
        &mut sender,
        mode,
        OTHER,
        10,
        1,
        Duration::from_millis(11),
        &[RECEIVED],
    );
    assert_eq!(sender.observations().bytes_in_flight, 0);
    assert_eq!(sender.counters().received, 2);
}

#[test]
fn submission_is_preflighted_funded_and_allocation_free() {
    for mode in [FeedbackMode::Continuous, FeedbackMode::Independent] {
        let mut sender = sender(mode);
        let first = context(mode, SPACE, 0);
        assert_eq!(
            sender.commit_preflighted(first),
            Err(SenderError::Exhausted)
        );
        assert_eq!(sender.observations().bytes_in_flight, 0);
        for packet in 0..7 {
            let context = context(mode, SPACE, packet);
            sender.prepare_commit(context).unwrap();
            assert_eq!(
                allocations(|| {
                    sender.preflight_commit(context).unwrap();
                    sender.commit_preflighted(context).unwrap();
                }),
                0
            );
            sender.assert_storage();
        }
        let flight = sender.observations().bytes_in_flight;
        assert_eq!(flight, 7 * 123);
        if let Ok(wire_len) = usize::try_from(u64::from(u32::MAX) + 1) {
            let oversized = SendCommit {
                wire_len,
                ..context(mode, OTHER, 100)
            };
            assert_eq!(
                sender.prepare_commit(oversized),
                Err(SenderError::InvalidCommit)
            );
        }
        assert_eq!(
            sender.prepare_commit(context(mode, SPACE, 7)),
            Err(SenderError::Exhausted)
        );
        assert_eq!(sender.observations().bytes_in_flight, flight);
        assert_eq!(sender.next_sent_id, 7);
        assert_eq!(
            sender.prepare_commit(first),
            Err(SenderError::InvalidCommit)
        );
        let wrong_epoch = SendCommit {
            path_epoch: Epoch(71),
            ..context(mode, OTHER, 100)
        };
        assert_eq!(
            sender.prepare_commit(wrong_epoch),
            Err(SenderError::InvalidCommit)
        );
        assert_eq!(sender.observations().bytes_in_flight, flight);
    }
}

#[test]
fn unknown_future_wrong_epoch_and_oversized_reports_cannot_finance_sends() {
    for mode in [FeedbackMode::Continuous, FeedbackMode::Independent] {
        let mut sender = sender(mode);
        send(&mut sender, context(mode, SPACE, 10));
        for (space, base, statuses) in [
            (OTHER, 90, &[RECEIVED][..]),
            (SPACE, 11, &[RECEIVED][..]),
            (SPACE, 10, &[RECEIVED; 8][..]),
        ] {
            feedback(
                &mut sender,
                mode,
                space,
                base,
                1,
                Duration::from_millis(10),
                statuses,
            );
            assert_eq!(sender.observations().bytes_in_flight, 123);
            assert!(!sender.observations().fresh_network_feedback);
        }
        feedback(
            &mut sender,
            mode,
            SPACE,
            10,
            1,
            Duration::from_millis(10),
            &[SendStatus {
                received: false,
                ..RECEIVED
            }],
        );
        assert_eq!(sender.observations().bytes_in_flight, 123);
        assert!(!sender.observations().fresh_network_feedback);
        if mode == FeedbackMode::Independent {
            let reports = [IndependentReport {
                space: SPACE,
                base: PacketId(10),
                generation: 1,
                statuses: &[],
            }; 8];
            sender.process_feedback(SendFeedback {
                received_at: Duration::from_millis(10),
                path_epoch: EPOCH,
                report: SendReport::Independent { reports: &reports },
            });
            assert_eq!(sender.observations().bytes_in_flight, 123);
            assert!(!sender.observations().fresh_network_feedback);
        }
        let statuses = [RECEIVED];
        let reports = [IndependentReport {
            space: SPACE,
            base: PacketId(10),
            generation: 1,
            statuses: &statuses,
        }];
        sender.process_feedback(SendFeedback {
            received_at: Duration::from_millis(10),
            path_epoch: Epoch(71),
            report: if mode == FeedbackMode::Continuous {
                SendReport::Continuous {
                    base_sequence: PacketId(10),
                    reference_micros: 100,
                    generation: 1,
                    statuses: &statuses,
                }
            } else {
                SendReport::Independent { reports: &reports }
            },
        });
        assert_eq!(sender.counters().wrong_path_feedback, 1);
        assert_eq!(sender.observations().bytes_in_flight, 123);
        feedback(
            &mut sender,
            mode,
            SPACE,
            10,
            2,
            Duration::from_millis(11),
            &[RECEIVED],
        );
        assert_eq!(sender.observations().bytes_in_flight, 0);
    }
}

#[test]
fn silence_expiry_budget_retirement_and_clear_abort_lifetimes() {
    let mode = FeedbackMode::Independent;
    let mut sender = sender(mode);
    for packet in 0..7 {
        send(&mut sender, context(mode, SPACE, packet));
    }
    let backing = sender.entries.as_ptr();
    let fixed = Storage::bytes(
        config().capacity,
        config().max_feedback_statuses,
        config().max_expirations_per_poll,
    )
    .unwrap();
    sender.retire_source(SPACE);
    assert_eq!(
        sender.prepare_commit(context(mode, SPACE, 8)),
        Err(SenderError::InvalidCommit)
    );
    let now = Duration::from_millis(100);
    for _ in 0..8 {
        let retired = sender.expire(now, config().max_expirations_per_poll);
        assert!(retired <= config().max_expirations_per_poll);
        assert!(!sender.observations().fresh_network_feedback);
        assert!(sender.observations().feedback.is_empty());
        sender.assert_storage();
        sender.clear_observations();
        if sender.next_deadline(now).is_none() {
            break;
        }
    }
    assert_eq!(sender.observations().bytes_in_flight, 0);
    assert_eq!(sender.counters().expired, 7);
    assert_eq!(sender.next_deadline(now), None);
    assert!(!sender.spaces.contains_key(&SPACE));
    sender.clear(Epoch(71));
    assert_eq!(sender.entries.as_ptr(), backing);
    assert!(sender.storage_bytes().unwrap() >= fixed);
    assert!(sender.storage_bytes().unwrap() <= sender.max_backing_bytes);
    let mut next = context(mode, OTHER, 500);
    next.path_epoch = Epoch(71);
    send(&mut sender, next);
    sender.abort();
    assert_eq!(sender.storage_bytes(), Some(0));
    assert_eq!(sender.next_deadline(now), None);
    assert_eq!(sender.observations().bytes_in_flight, 0);
    assert_eq!(sender.prepare_commit(next), Err(SenderError::InvalidCommit));
    sender.prepare_feedback_profile(mode).unwrap().commit();
    sender.set_availability(Epoch(72), true);
    next.path_epoch = Epoch(72);
    send(&mut sender, next);
}

#[test]
fn sparse_normalized_aliases_and_terminal_ids_do_not_expand_retirement_work() {
    let mode = FeedbackMode::Continuous;
    let mut settings = config();
    settings.capacity = 3;
    let mut sender = Sender::new(settings, mode, 64 * 1024).unwrap();
    sender.set_availability(EPOCH, true);
    send(&mut sender, context(mode, SPACE, 0));
    send(&mut sender, context(mode, SPACE, u64::MAX - 10));
    sender.clear_observations();
    sender.expire(settings.retention, 4);
    sender.clear_observations();
    send(&mut sender, context(mode, SPACE, u64::MAX - 9));
    send(&mut sender, context(mode, SPACE, u64::MAX - 8));
    assert_eq!(sender.oldest_feedback, Some(u64::MAX - 10));
    assert_eq!(sender.observations().bytes_in_flight, 246);
    let before = sender.counters();
    feedback(
        &mut sender,
        mode,
        SPACE,
        u64::MAX,
        5,
        Duration::from_millis(1),
        &[RECEIVED; 2],
    );
    assert!(sender.counters().unknown_feedback > before.unknown_feedback);
    assert_eq!(sender.observations().bytes_in_flight, 246);
}

#[test]
fn profile_reservation_is_atomic_at_the_actual_fixed_backing_limit() {
    let settings = config();
    let fixed = Storage::bytes(
        settings.capacity,
        settings.max_feedback_statuses,
        settings.max_expirations_per_poll,
    )
    .unwrap();
    let mut sender = Sender::new(settings, None, fixed - 1).unwrap();
    assert!(matches!(
        sender.prepare_feedback_profile(FeedbackMode::Continuous),
        Err(SenderError::Exhausted)
    ));
    assert_eq!(sender.storage_bytes(), Some(0));
    assert_eq!(sender.mode, None);
    let mut sender = Sender::new(settings, None, fixed).unwrap();
    drop(
        sender
            .prepare_feedback_profile(FeedbackMode::Continuous)
            .unwrap(),
    );
    assert_eq!(sender.storage_bytes(), Some(0));
    sender
        .prepare_feedback_profile(FeedbackMode::Continuous)
        .unwrap()
        .commit();
    assert_eq!(sender.storage_bytes(), Some(fixed));
    sender.set_availability(EPOCH, true);
    assert_eq!(
        sender.prepare_commit(context(FeedbackMode::Continuous, SPACE, 0)),
        Err(SenderError::Exhausted)
    );
    assert_eq!(sender.next_sent_id, 0);
    assert!(sender.spaces.is_empty());
    for settings in [
        SenderConfig {
            capacity: 0,
            ..settings
        },
        SenderConfig {
            capacity: usize::MAX,
            ..settings
        },
        SenderConfig {
            max_expirations_per_poll: 1,
            ..settings
        },
    ] {
        assert!(matches!(
            Sender::new(settings, None, usize::MAX),
            Err(SenderError::InvalidCommit)
        ));
    }
}
