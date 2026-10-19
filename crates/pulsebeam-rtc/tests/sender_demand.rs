#![allow(
    clippy::expect_used,
    reason = "public assignment fixtures stop at violated protocol invariants"
)]

mod support;

use std::time::Duration;

use pulsebeam_rtc::{Command, CommandError, MediaPayloadBitrate, PlayoutDelay, SourceIncarnation};
use support::{PeerFixture, forwarded};

#[test]
fn assignment_is_required_and_invalid_updates_preserve_the_previous_binding() {
    let mut fixture = PeerFixture::connected();
    let assignment = fixture.assignment(fixture.sender);
    fixture.command(Command::SetSenderAssignment {
        sender: fixture.sender,
        assignment: None,
    });
    let media = forwarded(fixture.send_source(b"explicit source binding"), 1);
    let unbound = fixture.connection.stats().senders;
    assert_eq!(
        fixture.try_command(Command::SendMedia {
            sender: fixture.sender,
            media: media.clone()
        }),
        Err(CommandError::SenderUnbound(fixture.sender))
    );
    assert_eq!(fixture.connection.stats().senders, unbound);

    fixture.command(Command::SetSenderAssignment {
        sender: fixture.sender,
        assignment: Some(assignment),
    });
    let mut invalid = assignment;
    invalid.potential_bitrate = MediaPayloadBitrate::from_bps(0);
    let before = fixture.connection.stats().senders;
    assert_eq!(
        fixture.try_command(Command::SetSenderAssignment {
            sender: fixture.sender,
            assignment: Some(invalid)
        }),
        Err(CommandError::InvalidSenderAssignment)
    );
    assert_eq!(fixture.connection.stats().senders, before);

    for field in 0..3 {
        let mut invalid = assignment;
        match field {
            0 => invalid.delivery.max_media_age = Duration::MAX,
            1 => invalid.delivery.max_queue_residence = Duration::MAX,
            _ => invalid.delivery.repair_window = Duration::MAX,
        }
        assert_eq!(
            fixture.try_command(Command::SetSenderAssignment {
                sender: fixture.sender,
                assignment: Some(invalid)
            }),
            Err(CommandError::InvalidSenderAssignment)
        );
        assert_eq!(fixture.connection.stats().senders, before);
    }

    let mut foreign_assignment = assignment;
    foreign_assignment.source = SourceIncarnation::from_value(2);
    assert_eq!(
        fixture.try_command(Command::RecoverSenderMedia {
            sender: fixture.sender,
            assignment: foreign_assignment
        }),
        Err(CommandError::SourceIncarnationMismatch)
    );
    assert_eq!(fixture.connection.stats().senders, before);
    let mut foreign = media.clone();
    foreign.source = SourceIncarnation::from_value(2);
    assert_eq!(
        fixture.try_command(Command::SendMedia {
            sender: fixture.sender,
            media: foreign
        }),
        Err(CommandError::SourceIncarnationMismatch)
    );
    assert_eq!(fixture.connection.stats().senders, before);
    fixture.command(Command::SendMedia {
        sender: fixture.sender,
        media,
    });
    assert_eq!(fixture.receive_egress().1, b"explicit source binding");
}

#[test]
fn finite_media_age_is_independent_of_wire_playout_intent() {
    let mut fixture = PeerFixture::connected();
    let media = forwarded(fixture.send_source(b"explicit age bound"), 1);
    let mut assignment = fixture.assignment(fixture.sender);
    assignment.delivery.max_media_age = Duration::from_millis(1);
    fixture.advance_clock_without_poll(Duration::from_millis(2));
    for playout in [
        None,
        Some(PlayoutDelay::ZERO),
        Some(PlayoutDelay::from_millis(0, 500).expect("wire range")),
    ] {
        assignment.playout_delay = playout;
        fixture.command(Command::SetSenderAssignment {
            sender: fixture.sender,
            assignment: Some(assignment),
        });
        let before = fixture.connection.stats().senders;
        assert_eq!(
            fixture.try_command(Command::SendMedia {
                sender: fixture.sender,
                media: media.clone()
            }),
            Err(CommandError::MediaExpired)
        );
        assert_eq!(fixture.connection.stats().senders, before);
    }
}

#[test]
fn potential_interest_cannot_admit_paused_media_and_complete_assignment_can_resume() {
    let mut fixture = PeerFixture::connected();
    let mut assignment = fixture.assignment(fixture.sender);
    assignment.selected_bitrate = MediaPayloadBitrate::from_bps(0);
    assignment.potential_bitrate = MediaPayloadBitrate::from_bps(2_000_000);
    let padding_before = fixture
        .connection
        .stats()
        .connection
        .transmitted_padding_bytes;
    fixture.command(Command::SetSenderAssignment {
        sender: fixture.sender,
        assignment: Some(assignment),
    });
    let packet = fixture.send_source(b"paused source");
    assert_eq!(
        fixture.try_command(Command::SendMedia {
            sender: fixture.sender,
            media: forwarded(packet, 1)
        }),
        Err(CommandError::WouldBlock)
    );
    fixture.drive_for(Duration::from_millis(100));
    let stats = fixture.connection.stats();
    let sender = stats.senders.first().expect("sender");
    assert_eq!(sender.assignment, Some(assignment));
    assert_eq!(sender.transmitted_payload_bytes, 0);
    assert!(stats.connection.transmitted_padding_bytes > padding_before);
    assert!(stats.connection.media_budget.as_bps() > 0);
    assignment.selected_bitrate = MediaPayloadBitrate::from_bps(256_000);
    fixture.command(Command::SetSenderAssignment {
        sender: fixture.sender,
        assignment: Some(assignment),
    });
    let mut admitted = false;
    for _ in 0..100 {
        let packet = fixture.send_source(b"selected source resumed");
        match fixture.try_command(Command::SendMedia {
            sender: fixture.sender,
            media: forwarded(packet, 2),
        }) {
            Ok(()) => {
                admitted = true;
                break;
            }
            Err(CommandError::WouldBlock) => fixture.drive_for(Duration::from_millis(2)),
            Err(error) => panic!("unexpected resume error: {error:?}"),
        }
    }
    assert!(admitted, "selected media resumes after bounded probe debt");
    assert_eq!(fixture.receive_egress().1, b"selected source resumed");
    assert_eq!(fixture.assignment(fixture.sender), assignment);
}
