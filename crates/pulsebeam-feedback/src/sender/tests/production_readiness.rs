use super::*;

#[test]
fn old_epoch_feedback_cannot_fund_reused_spaces_during_bounded_retirement() {
    let mode = FeedbackMode::Independent;
    let mut sender = sender(mode);
    let mut epoch = EPOCH;
    for cycle in 0..12_u64 {
        let now = Duration::from_millis(cycle * 200);
        let spaces = [SpaceId(10_000 + cycle * 2), SpaceId(10_001 + cycle * 2)];
        for space in spaces {
            send(
                &mut sender,
                SendCommit {
                    at: now,
                    path_epoch: epoch,
                    ..context(mode, space, 1)
                },
            );
        }
        let next = Epoch(epoch.0 + 1);
        sender.set_availability(next, true);
        sender.clear_observations();
        for space in spaces {
            send(
                &mut sender,
                SendCommit {
                    at: now,
                    path_epoch: next,
                    ..context(mode, space, 1)
                },
            );
        }
        let reports = spaces.map(|space| IndependentReport {
            space,
            base: PacketId(1),
            generation: 1,
            statuses: &[RECEIVED],
        });
        sender.process_feedback(SendFeedback {
            received_at: now,
            path_epoch: epoch,
            report: SendReport::Independent { reports: &reports },
        });
        assert_eq!(sender.observations().bytes_in_flight, 246);
        assert!(sender.observations().feedback.is_empty());
        assert!(!sender.observations().fresh_network_feedback);
        sender.process_feedback(SendFeedback {
            received_at: now,
            path_epoch: next,
            report: SendReport::Independent { reports: &reports },
        });
        assert_eq!(sender.observations().bytes_in_flight, 0);
        assert_eq!(
            sender
                .observations()
                .feedback
                .iter()
                .filter(|sample| sample.newly_acked)
                .count(),
            2
        );
        assert!(sender.observations().fresh_network_feedback);
        sender.clear_observations();
        sender.process_feedback(SendFeedback {
            received_at: now,
            path_epoch: next,
            report: SendReport::Independent { reports: &reports },
        });
        assert!(sender.observations().feedback.is_empty());
        assert!(!sender.observations().fresh_network_feedback);
        for space in spaces {
            sender.retire_source(space);
        }
        for _ in 0..8 {
            assert!(
                sender.expire(
                    now + Duration::from_millis(150),
                    config().max_expirations_per_poll
                ) <= config().max_expirations_per_poll
            );
            sender.clear_observations();
        }
        assert!(sender.storage_bytes().unwrap() <= 64 * 1024);
        epoch = next;
    }
    assert_eq!(sender.counters().wrong_path_feedback, 24);
}
