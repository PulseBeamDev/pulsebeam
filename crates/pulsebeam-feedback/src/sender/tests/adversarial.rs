use super::*;

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

#[test]
fn stale_normalized_ranges_cannot_advance_either_credit_edge() {
    for mode in [FeedbackMode::Continuous, FeedbackMode::Independent] {
        let settings = SenderConfig {
            max_ack_reordering: 2,
            ..config()
        };
        let mut sender = Sender::new(settings, mode, 64 * 1024).unwrap();
        sender.set_availability(EPOCH, true);
        for packet in 0..4 {
            send(&mut sender, context(mode, SPACE, packet));
        }
        sender.clear_observations();
        feedback(&mut sender, mode, SPACE, 0, 1, ms(10), &[RECEIVED]);
        assert_eq!(sender.counters.stale_feedback, 1);
        assert_eq!(sender.bytes_in_flight, 4 * 123);
        assert!(sender.inputs.feedback.is_empty());
        assert!(!sender.inputs.fresh_network_feedback);
        assert_eq!(sender.highest_feedback_acked, None);
        assert_eq!(sender.spaces[&SPACE].highest_acked_sequence, None);
    }
}

#[test]
fn epoch_change_lazily_resets_a_space_absent_from_the_retained_ring() {
    let mode = FeedbackMode::Independent;
    let mut sender = sender(mode);
    send(&mut sender, context(mode, SPACE, 10));
    feedback(&mut sender, mode, SPACE, 10, 1_000, ms(1), &[RECEIVED]);
    for offset in 1..=7 {
        sender.clear_observations();
        send(
            &mut sender,
            SendCommit {
                at: ms(offset * 2),
                ..context(mode, OTHER, 20 + offset)
            },
        );
        feedback(
            &mut sender,
            mode,
            OTHER,
            20 + offset,
            1_000 + i64::try_from(offset).unwrap(),
            ms(offset * 2 + 1),
            &[RECEIVED],
        );
    }
    assert!(sender.spaces[&SPACE].by_sequence.is_empty());
    assert_eq!(
        sender.spaces[&SPACE].last_report_generation,
        Some((EPOCH, 1_000))
    );
    sender.clear_observations();
    while sender.oldest_sent_id < sender.next_sent_id {
        assert!(sender.expire(ms(120), 4) > 0);
        sender.clear_observations();
    }
    assert_eq!(sender.next_deadline(ms(120)), None);
    let next = Epoch(71);
    sender.set_availability(next, true);
    assert_eq!(
        sender.spaces[&SPACE].last_report_generation,
        Some((EPOCH, 1_000))
    );
    assert_eq!(sender.space_anchor(SPACE).unwrap().1, None);
    send(
        &mut sender,
        SendCommit {
            at: ms(130),
            path_epoch: next,
            ..context(mode, SPACE, 10)
        },
    );
    assert_eq!(sender.spaces[&SPACE].last_report_generation, None);
    assert_eq!(sender.spaces[&SPACE].highest_acked_sequence, None);
    let reports = [IndependentReport {
        space: SPACE,
        base: PacketId(10),
        generation: 1,
        statuses: &[RECEIVED],
    }];
    sender.process_feedback(SendFeedback {
        received_at: ms(131),
        path_epoch: next,
        report: SendReport::Independent { reports: &reports },
    });
    assert_eq!(sender.bytes_in_flight, 0);
    assert_eq!(sender.inputs.feedback.len(), 1);
    assert!(sender.inputs.feedback[0].newly_acked);
    assert!(sender.inputs.fresh_network_feedback);
    sender.assert_storage();
}

#[test]
fn independent_source_funding_growth_and_terminal_retirement() {
    let mode = FeedbackMode::Independent;
    let mut sender = sender(mode);
    let first = context(mode, SPACE, 1);
    let fixed = sender.assert_storage();
    let minimum = HashMap::<u64, SentPacketId>::with_capacity(1).capacity();
    let source_bytes = SOURCE_PAYLOAD_BYTES + minimum * SOURCE_INDEX_ENTRY_BYTES;
    sender.max_backing_bytes = fixed + source_bytes - 1;
    assert_eq!(sender.prepare_commit(first), Err(SenderError::Exhausted));
    assert!(sender.spaces.is_empty());
    assert_eq!(sender.next_sent_id, 0);
    assert_eq!(sender.assert_storage(), fixed);
    sender.max_backing_bytes += 1;
    sender.prepare_commit(first).unwrap();
    assert_eq!(sender.spaces[&SPACE].index_capacity, minimum);
    assert!(sender.spaces[&SPACE].by_sequence.is_empty());
    assert_eq!(sender.bytes_in_flight, 0);
    sender.commit_preflighted(first).unwrap();
    let retained = sender.assert_storage();
    let grown = HashMap::<u64, SentPacketId>::with_capacity(minimum + 1).capacity();
    sender.max_backing_bytes = retained + (grown - minimum) * SOURCE_INDEX_ENTRY_BYTES - 1;
    assert_eq!(
        sender.reserve_source_capacity(first.identity, minimum + 1),
        Err(SenderError::Exhausted)
    );
    assert_eq!(sender.spaces[&SPACE].index_capacity, minimum);
    assert_eq!(sender.assert_storage(), retained);
    sender.max_backing_bytes += 1;
    sender
        .reserve_source_capacity(first.identity, minimum + 1)
        .unwrap();
    assert_eq!(sender.spaces[&SPACE].index_capacity, grown);
    assert_eq!(
        sender.spaces[&SPACE].by_sequence.get(&1),
        Some(&SentPacketId(0))
    );
    let entry = *sender.entry(SentPacketId(0)).unwrap();
    sender.remove_indexes(SentEntry {
        id: SentPacketId(99),
        ..entry
    });
    assert_eq!(
        sender.spaces[&SPACE].by_sequence.get(&1),
        Some(&SentPacketId(0))
    );
    sender.retire_source(SPACE);
    sender.retire_source(SPACE);
    assert_eq!(
        sender.prepare_commit(context(mode, SPACE, 2)),
        Err(SenderError::InvalidCommit)
    );
    feedback(&mut sender, mode, SPACE, 1, 1, ms(10), &[RECEIVED]);
    assert_eq!(sender.bytes_in_flight, 0);
    sender.clear_observations();
    sender.expire(ms(99), 4);
    assert!(sender.spaces.contains_key(&SPACE));
    sender.clear_observations();
    sender.expire(ms(100), 4);
    assert!(!sender.spaces.contains_key(&SPACE));
    assert_eq!(sender.assert_storage(), fixed);
}

#[test]
fn learned_window_cannot_reopen_a_budget_deferred_deadline() {
    let mode = FeedbackMode::Continuous;
    let mut sender = sender(mode);
    for packet in 0..3 {
        send(&mut sender, context(mode, SPACE, packet));
    }
    feedback(&mut sender, mode, SPACE, 0, 1, ms(1), &[MISSING; 3]);
    feedback(
        &mut sender,
        mode,
        SPACE,
        0,
        2,
        ms(10),
        &[MISSING, MISSING, RECEIVED],
    );
    sender.clear_observations();
    assert_eq!(sender.confirm_losses(ms(17), 1), 1);
    sender.loss_work = config().max_expirations_per_poll;
    feedback(&mut sender, mode, SPACE, 0, 3, ms(20), &[RECEIVED]);
    assert_eq!(sender.reordering_window, ms(15));
    assert_eq!(sender.next_deadline(ms(20)), Some(ms(17)));
    assert_eq!(
        sender
            .entry(SentPacketId(1))
            .unwrap()
            .frozen_reorder_deadline,
        Some(ms(17))
    );
    sender.clear_observations();
    sender.confirm_losses(ms(20), 4);
    assert_eq!(sender.counters.not_received, 2);
}

#[test]
fn synthetic_capacity_never_evicts_network_evidence() {
    let mode = FeedbackMode::Continuous;
    let mut sender = sender(mode);
    send(&mut sender, context(mode, SPACE, 0));
    sender.emit(SentPacketId(0), true, true, None, RECEIVED.arrival, None);
    for _ in 0..=config().max_expirations_per_poll {
        sender.emit(
            SentPacketId(0),
            false,
            false,
            Some(LossOrigin::Reordering),
            None,
            None,
        );
    }
    assert_eq!(sender.inputs.feedback.len(), 1);
    assert!(sender.inputs.feedback[0].received);
    assert_eq!(
        sender.inputs.synthetic.len(),
        config().max_expirations_per_poll
    );
    assert!(!sender.inputs.fresh_network_feedback);
}

#[test]
fn terminal_loss_and_native_recovery_are_distinct() {
    let mode = FeedbackMode::Independent;
    let mut sender = sender(mode);
    send(&mut sender, context(mode, SPACE, 1));
    sender.retire_source(SPACE);
    sender.set_missing(SentPacketId(0), Duration::ZERO);
    sender.anchor_missing(None, Some((SPACE, 2)), Duration::ZERO);
    sender.clear_observations();
    feedback(&mut sender, mode, SPACE, 1, 1, ms(7), &[RECEIVED]);
    assert!(sender.inputs.fresh_network_feedback);
    assert_eq!(sender.inputs.timing.unwrap().feedback_hold, RECEIVED.hold);
    assert_eq!(
        sender.entry(SentPacketId(0)).unwrap().acknowledgment,
        Acknowledgment::Lost
    );
    assert_eq!(sender.counters.not_received, 1);
    assert_eq!(sender.counters.received, 0);
    let recovery = sender.inputs.feedback.iter().find(|s| s.recovered).unwrap();
    assert!(recovery.received && recovery.newly_acked);
}

#[test]
fn terminal_replay_does_not_refresh_live_missing() {
    for mode in [FeedbackMode::Continuous, FeedbackMode::Independent] {
        let mut sender = sender(mode);
        for packet in 0..3 {
            send(&mut sender, context(mode, SPACE, packet));
        }
        feedback(
            &mut sender,
            mode,
            SPACE,
            0,
            1,
            Duration::ZERO,
            &[MISSING, RECEIVED, MISSING],
        );
        sender.clear_observations();
        feedback(
            &mut sender,
            mode,
            SPACE,
            0,
            1,
            ms(7),
            &[RECEIVED, MISSING, MISSING],
        );
        assert!(!sender.inputs.fresh_network_feedback);
        assert!(!sender.inputs.feedback.iter().any(|s| s.received));
        assert_eq!(
            sender.entry(SentPacketId(0)).unwrap().acknowledgment,
            Acknowledgment::Lost
        );
    }
}

#[test]
fn independent_output_exhaustion_keeps_backing_bounded() {
    let mode = FeedbackMode::Independent;
    let settings = SenderConfig {
        max_feedback_statuses: 2,
        ..config()
    };
    let mut sender = Sender::new(settings, mode, 64 * 1024).unwrap();
    sender.set_availability(EPOCH, true);
    for space in [SPACE, OTHER] {
        for packet in 0..2 {
            send(&mut sender, context(mode, space, packet));
        }
    }
    sender.clear_observations();
    let backing = sender.storage_bytes().unwrap();
    sender.max_backing_bytes = backing + size_of::<SentObservation>() + 2 * MISSING_SLOT_BYTES;
    let statuses = [RECEIVED];
    let reports = [SPACE, OTHER].map(|space| IndependentReport {
        space,
        base: PacketId(1),
        generation: 1,
        statuses: &statuses,
    });
    sender.process_feedback(SendFeedback {
        received_at: ms(10),
        path_epoch: EPOCH,
        report: SendReport::Independent { reports: &reports },
    });
    assert!(sender.inputs.exhausted);
    assert_eq!(sender.inputs.feedback.len(), 3);
    assert_eq!(sender.bytes_in_flight, 0);
    assert_eq!(
        sender.entry(SentPacketId(3)).unwrap().acknowledgment,
        Acknowledgment::Received
    );
    assert_eq!(sender.counters.received, 2);
    assert!(sender.storage_bytes().unwrap() <= sender.max_backing_bytes);
    assert!(sender.inputs.feedback.iter().all(|s| s.newly_acked));
    let counters = sender.counters;
    sender.process_feedback(SendFeedback {
        received_at: ms(11),
        path_epoch: EPOCH,
        report: SendReport::Independent { reports: &reports },
    });
    assert_eq!(sender.counters, counters);
    assert_eq!(sender.inputs.feedback.len(), 3);
    sender.clear(Epoch(71));
    assert!(!sender.inputs.exhausted);
    send(
        &mut sender,
        SendCommit {
            at: ms(12),
            path_epoch: Epoch(71),
            ..context(mode, OTHER, 42)
        },
    );
    assert_eq!(sender.bytes_in_flight, 123);
}

#[test]
fn generated_overlap_credits_each_send_once() {
    for size in 1..128_usize {
        for pattern in 0..5 {
            let settings = SenderConfig {
                capacity: size,
                max_feedback_statuses: size,
                max_ack_reordering: 127,
                ..config()
            };
            let mode = FeedbackMode::Continuous;
            let mut sender = Sender::new(settings, mode, 1024 * 1024).unwrap();
            sender.set_availability(EPOCH, true);
            sender.clear_observations();
            for packet in 0..size {
                send(&mut sender, context(mode, SPACE, packet as u64));
            }
            let statuses: Vec<_> = (0..size)
                .map(|index| {
                    if match pattern {
                        0 => false,
                        1 => true,
                        2 => index % 2 == 0,
                        3 => index % 3 == 1,
                        _ => index >= size / 2,
                    } {
                        RECEIVED
                    } else {
                        MISSING
                    }
                })
                .collect();
            feedback(&mut sender, mode, SPACE, 0, 1, ms(10), &statuses);
            let first = sender
                .inputs
                .feedback
                .iter()
                .filter(|s| s.newly_acked)
                .count();
            sender.clear_observations();
            feedback(
                &mut sender,
                mode,
                SPACE,
                0,
                2,
                ms(11),
                &vec![RECEIVED; size],
            );
            let second = sender
                .inputs
                .feedback
                .iter()
                .filter(|s| s.newly_acked)
                .count();
            assert_eq!(first + second, size, "size={size} pattern={pattern}");
            assert_eq!(sender.bytes_in_flight, 0);
            assert_eq!(sender.counters.received, size as u64);
            assert!(sender.inputs.feedback.len() <= size);
            assert!(sender.storage_bytes().unwrap() <= sender.max_backing_bytes);
            assert!(!sender.inputs.exhausted);
        }
    }
}

#[test]
fn unreported_prefix_credit_is_not_receipt_or_missing() {
    let mode = FeedbackMode::Continuous;
    let mut sender = sender(mode);
    for packet in 0..3 {
        send(&mut sender, context(mode, SPACE, packet));
    }
    feedback(&mut sender, mode, SPACE, 2, 1, ms(10), &[RECEIVED]);
    assert_eq!(sender.bytes_in_flight, 0);
    assert_eq!(
        sender
            .inputs
            .feedback
            .iter()
            .filter(|s| s.newly_acked)
            .count(),
        3
    );
    for id in 0..2 {
        assert_eq!(
            sender.entry(SentPacketId(id)).unwrap().acknowledgment,
            Acknowledgment::Pending
        );
    }
    assert!(sender.missing.is_empty());
    sender.clear_observations();
    feedback(&mut sender, mode, SPACE, 0, 2, ms(11), &[RECEIVED; 2]);
    assert_eq!(sender.inputs.feedback.len(), 2);
    assert!(
        sender
            .inputs
            .feedback
            .iter()
            .all(|s| s.received && !s.newly_acked && !s.lost)
    );
    assert_eq!(sender.counters.received, 3);
}

#[test]
fn missing_without_higher_receipt_has_no_reorder_deadline() {
    for mode in [FeedbackMode::Continuous, FeedbackMode::Independent] {
        let mut sender = sender(mode);
        for packet in 0..3 {
            send(&mut sender, context(mode, SPACE, packet));
        }
        feedback(&mut sender, mode, SPACE, 0, 1, ms(10), &[MISSING; 2]);
        assert_eq!(sender.bytes_in_flight, 369);
        assert_eq!(sender.next_deadline(ms(10)), Some(ms(100)));
        assert_eq!(sender.counters.not_received, 0);
        assert_eq!(sender.inputs.feedback.len(), 2);
        assert!(sender.inputs.feedback.iter().all(|s| !s.received
            && !s.newly_acked
            && !s.lost
            && s.reported_lost_for_the_first_time == (mode == FeedbackMode::Independent)));
        sender.clear_observations();
        feedback(
            &mut sender,
            mode,
            SPACE,
            0,
            2,
            ms(20),
            &[MISSING, MISSING, RECEIVED],
        );
        assert_eq!(sender.next_deadline(ms(20)), Some(ms(27)));
        sender.clear_observations();
        sender.expire(ms(26), 4);
        assert_eq!(sender.counters.not_received, 0);
        sender.clear_observations();
        sender.expire(ms(27), 4);
        assert_eq!(sender.counters.not_received, 2);
        assert!(
            sender
                .inputs
                .synthetic
                .iter()
                .all(|s| s.loss_origin == Some(LossOrigin::Reordering))
        );
    }
}

#[test]
fn missing_debt_requires_funding_and_preserves_credit() {
    let mode = FeedbackMode::Continuous;
    let mut sender = sender(mode);
    for packet in 0..3 {
        send(&mut sender, context(mode, SPACE, packet));
    }
    let backing = sender.storage_bytes().unwrap();
    sender.max_backing_bytes = backing + MISSING_SLOT_BYTES - 1;
    assert!(sender.reserve_missing_slot().is_none());
    assert_eq!(sender.storage_bytes(), Some(backing));
    assert!(sender.missing.is_empty());
    sender.max_backing_bytes = backing + 3 * MISSING_SLOT_BYTES;
    feedback(&mut sender, mode, SPACE, 0, 1, ms(10), &[MISSING; 3]);
    assert_eq!(sender.bytes_in_flight, 369);
    assert_eq!(sender.missing.len(), 3);
    assert!(sender.storage_bytes().unwrap() <= sender.max_backing_bytes);
    sender.clear_observations();
    feedback(
        &mut sender,
        mode,
        SPACE,
        0,
        2,
        ms(20),
        &[MISSING, RECEIVED, MISSING],
    );
    assert_eq!(sender.bytes_in_flight, 123);
    assert_eq!(
        sender
            .inputs
            .feedback
            .iter()
            .filter(|s| s.newly_acked)
            .count(),
        2
    );
    assert_eq!(
        sender.inputs.feedback.iter().filter(|s| s.received).count(),
        1
    );
}

#[test]
fn loss_and_expiry_share_one_observation_cycle_budget() {
    let mode = FeedbackMode::Continuous;
    let mut sender = sender(mode);
    for packet in 0..7 {
        send(&mut sender, context(mode, SPACE, packet));
    }
    sender.clear_observations();
    assert_eq!(sender.expire(ms(100), 4), 4);
    assert_eq!(sender.loss_work, 4);
    feedback(&mut sender, mode, SPACE, 0, 1, ms(100), &[RECEIVED; 7]);
    assert_eq!(sender.loss_work, 4);
    assert_eq!(sender.inputs.synthetic.len(), 4);
    assert!(
        sender
            .inputs
            .synthetic
            .iter()
            .all(|s| s.loss_origin == Some(LossOrigin::HistoryExpiry))
    );
    assert!(sender.inputs.feedback.is_empty());
    assert!(!sender.inputs.fresh_network_feedback);
    assert_eq!(sender.bytes_in_flight, 3 * 123);
    sender.clear_observations();
    assert_eq!(sender.expire(ms(100), 4), 3);
    assert_eq!(sender.bytes_in_flight, 0);
}

#[test]
fn invalid_gap_does_not_poison_continuous_baseline() {
    let mode = FeedbackMode::Continuous;
    let mut sender = sender(mode);
    for packet in [1, 3] {
        send(&mut sender, context(mode, SPACE, packet));
    }
    feedback(&mut sender, mode, SPACE, 1, 1, ms(10), &[RECEIVED]);
    sender.clear_observations();
    let baseline = (
        sender.feedback_reference,
        sender.feedback_generation,
        sender.highest_feedback_acked,
    );
    feedback(
        &mut sender,
        mode,
        SPACE,
        2,
        200,
        ms(20),
        &[MISSING, RECEIVED],
    );
    assert_eq!(
        (
            sender.feedback_reference,
            sender.feedback_generation,
            sender.highest_feedback_acked
        ),
        baseline
    );
    assert_eq!(sender.bytes_in_flight, 123);
    assert!(sender.inputs.feedback.is_empty());
    assert!(sender.inputs.timing.is_none());
    assert!(!sender.inputs.fresh_network_feedback);
    feedback(&mut sender, mode, SPACE, 3, 2, ms(21), &[RECEIVED]);
    assert_eq!(sender.bytes_in_flight, 123);
    assert!(sender.inputs.feedback.is_empty());
    assert!(!sender.inputs.fresh_network_feedback);
    assert_eq!(sender.counters.unknown_feedback, 2);
    assert_eq!(
        (
            sender.feedback_reference,
            sender.feedback_generation,
            sender.highest_feedback_acked
        ),
        baseline
    );
    sender.clear_observations();
    sender.expire(config().retention, 4);
    assert_eq!(sender.bytes_in_flight, 0);
    assert!(sender.inputs.feedback.is_empty());
    assert!(!sender.inputs.fresh_network_feedback);
    assert_eq!(sender.counters.not_received, 1);
}

#[test]
fn unavailable_path_evidence_survives_repetition_and_path_change_resets_indexes() {
    let mode = FeedbackMode::Independent;
    let mut sender = sender(mode);
    send(&mut sender, context(mode, SPACE, 1));
    feedback(&mut sender, mode, SPACE, 1, 1, ms(10), &[RECEIVED]);
    sender.set_availability(EPOCH, false);
    sender.set_availability(EPOCH, false);
    assert_eq!(
        sender.inputs.path_change,
        Some(AvailabilityChange {
            epoch: EPOCH,
            available: false
        })
    );
    let next = Epoch(71);
    sender.set_availability(next, true);
    assert!(sender.inputs.feedback.is_empty());
    assert!(sender.inputs.synthetic.is_empty());
    assert!(sender.inputs.timing.is_none());
    assert!(sender.spaces[&SPACE].by_sequence.is_empty());
    let mut packet = context(mode, SPACE, 500);
    packet.path_epoch = next;
    send(&mut sender, packet);
    assert_eq!(sender.spaces[&SPACE].first_sequence, 500);
}

#[test]
fn terminal_receipt_does_not_replace_new_receipt_hold() {
    let mode = FeedbackMode::Independent;
    let mut sender = sender(mode);
    for packet in 0..2 {
        send(&mut sender, context(mode, SPACE, packet));
    }
    feedback(&mut sender, mode, SPACE, 0, 1, ms(10), &[RECEIVED]);
    sender.clear_observations();
    let old = SendStatus {
        hold: Some(ms(1)),
        ..RECEIVED
    };
    let new = SendStatus {
        hold: Some(ms(9)),
        arrival: Some(ReceiverTime::Micros(500)),
        ..RECEIVED
    };
    feedback(&mut sender, mode, SPACE, 0, 2, ms(20), &[old, new]);
    assert_eq!(sender.inputs.feedback.len(), 1);
    assert_eq!(sender.inputs.feedback[0].receiver_arrival, new.arrival);
    assert_eq!(sender.inputs.timing.unwrap().feedback_hold, Some(ms(9)));
}

#[test]
fn expired_feedback_backlog_cannot_spend_the_same_work_budget_twice() {
    let mode = FeedbackMode::Continuous;
    let settings = SenderConfig {
        capacity: 8,
        max_feedback_statuses: 8,
        ..config()
    };
    let mut sender = Sender::new(settings, mode, 64 * 1024).unwrap();
    sender.set_availability(EPOCH, true);
    for packet in 0..8 {
        send(&mut sender, context(mode, SPACE, packet));
    }
    sender.clear_observations();
    assert_eq!(sender.expire(ms(100), 4), 4);
    feedback(&mut sender, mode, SPACE, 0, 1, ms(100), &[RECEIVED; 8]);
    assert_eq!(sender.loss_work, 4);
    assert_eq!(sender.inputs.synthetic.len(), 4);
    assert!(sender.inputs.synthetic.iter().all(|sample| sample.lost
        && sample.loss_origin == Some(LossOrigin::HistoryExpiry)
        && !sample.reported_lost_for_the_first_time));
    assert!(sender.inputs.feedback.is_empty());
    assert!(!sender.inputs.fresh_network_feedback);
    assert!(!sender.inputs.exhausted);
    assert_eq!(sender.bytes_in_flight, 4 * 123);
    assert!(
        sender
            .next_deadline(ms(100))
            .is_some_and(|deadline| deadline <= ms(100))
    );
    sender.clear_observations();
    assert_eq!(sender.expire(ms(100), 4), 4);
    assert_eq!(sender.inputs.synthetic.len(), 4);
    assert_eq!(sender.bytes_in_flight, 0);
    assert_eq!(sender.counters.expired, 8);
}

#[test]
fn expiry_backlog_preserves_an_earlier_reordering_confirmation() {
    let mode = FeedbackMode::Continuous;
    let settings = SenderConfig {
        capacity: 9,
        max_feedback_statuses: 9,
        ..config()
    };
    let mut sender = Sender::new(settings, mode, 64 * 1024).unwrap();
    sender.set_availability(EPOCH, true);
    for packet in 0..9 {
        send(&mut sender, context(mode, SPACE, packet));
    }
    let mut statuses = [MISSING; 9];
    statuses[8] = RECEIVED;
    feedback(&mut sender, mode, SPACE, 0, 1, ms(10), &statuses);
    sender.clear_observations();
    sender.missing.make_contiguous().reverse();
    assert_eq!(sender.expire(ms(100), 4), 4);
    assert_eq!(sender.inputs.synthetic.len(), 4);
    assert!(sender.inputs.synthetic.iter().all(|sample| sample.lost
        && sample.loss_origin == Some(LossOrigin::Reordering)
        && !sample.reported_lost_for_the_first_time));
    assert_eq!(sender.bytes_in_flight, 0);
    sender.assert_storage();
}

#[test]
fn learned_reordering_window_is_bounded_and_monotonic_after_confirmation() {
    let mode = FeedbackMode::Independent;
    let mut sender = sender(mode);
    send(&mut sender, context(mode, SPACE, 1));
    sender.set_missing(SentPacketId(0), Duration::ZERO);
    sender.recover(Some(SentPacketId(0)), EPOCH, None, None, ms(80));
    sender.apply_reordering_window(ms(80));
    assert_eq!(sender.reordering_window, config().max_reordering_window);
    send(
        &mut sender,
        SendCommit {
            at: ms(80),
            ..context(mode, SPACE, 2)
        },
    );
    sender.set_missing(SentPacketId(1), ms(80));
    assert_eq!(sender.confirm_losses(ms(95), 2), 2);
    assert_eq!(
        sender.entry(SentPacketId(1)).unwrap().acknowledgment,
        Acknowledgment::Missing {
            since: ms(80),
            higher_received_at: None
        }
    );
    sender.anchor_missing(None, Some((SPACE, 3)), ms(95));
    sender.confirm_losses(ms(110), 2);
    assert_eq!(sender.reordering_window, config().max_reordering_window);
    assert_eq!(sender.counters.not_received, 1);
}

#[test]
fn repeated_profile_activation_preserves_funded_pending_history() {
    let mode = FeedbackMode::Continuous;
    let mut sender = sender(mode);
    send(&mut sender, context(mode, SPACE, 0));
    feedback(&mut sender, mode, SPACE, 0, 1, ms(10), &[MISSING]);
    let backing = sender.entries.as_ptr();
    let bytes = sender.assert_storage();
    let observations = sender.inputs.feedback.len();
    let counters = sender.counters;
    assert_eq!(
        allocations(|| sender.prepare_feedback_profile(mode).unwrap().commit()),
        0
    );
    assert_eq!(sender.entries.as_ptr(), backing);
    assert_eq!(sender.assert_storage(), bytes);
    assert_eq!(sender.next_sent_id, 1);
    assert_eq!(sender.bytes_in_flight, 123);
    assert_eq!(sender.inputs.feedback.len(), observations);
    assert_eq!(sender.counters, counters);
    assert!(matches!(
        sender.entry(SentPacketId(0)).unwrap().acknowledgment,
        Acknowledgment::Missing { .. }
    ));
}
