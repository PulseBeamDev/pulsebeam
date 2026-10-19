#![allow(
    clippy::disallowed_types,
    clippy::expect_used,
    reason = "sender cleanup fixtures stop at violated public protocol invariants"
)]

mod support;

use std::time::Duration;

use pulsebeam_rtc::{Command, CommandError, FrameBoundary, MediaPayloadBitrate};
use support::{NetworkPolicy, PeerFixture, RtpImpairment, forwarded};

#[test]
fn replacement_fences_old_source_without_rewinding_committed_continuity() {
    let mut fixture = PeerFixture::connected();
    let first = fixture.send_source(b"committed source");
    fixture.command(Command::SendMedia {
        sender: fixture.sender,
        media: forwarded(first, 1),
    });
    let (before, payload) = fixture.receive_egress();
    assert_eq!(payload, b"committed source");

    let stale = fixture.send_source(b"discarded source");
    let replacement = fixture.send_source(b"replacement source");
    fixture.command(Command::SendMedia {
        sender: fixture.sender,
        media: forwarded(stale, 2),
    });
    assert_eq!(
        fixture
            .connection
            .stats()
            .senders
            .first()
            .expect("sender")
            .queued_packets,
        1
    );
    let mut assignment = fixture.assignment(fixture.sender);
    assignment.source = pulsebeam_rtc::SourceIncarnation::from_value(2);
    fixture.command(Command::SetSenderAssignment {
        sender: fixture.sender,
        assignment: Some(assignment),
    });
    let stats = fixture.connection.stats();
    let sender = stats.senders.first().expect("sender");
    assert_eq!(sender.queued_packets, 0);
    assert_eq!(sender.queued_payload_bytes, 0);
    assert_eq!(sender.transmitted_packets, 1);
    assert_eq!(
        fixture.try_command(Command::SendMedia {
            sender: fixture.sender,
            media: forwarded(replacement.clone(), 3)
        }),
        Err(CommandError::SourceIncarnationMismatch)
    );
    let mut media = forwarded(replacement, 3);
    media.source = assignment.source;
    fixture.command(Command::SendMedia {
        sender: fixture.sender,
        media,
    });
    let (after, payload) = fixture.receive_egress();
    assert_eq!(payload, b"replacement source");
    assert_eq!(after.ssrc, before.ssrc);
    assert_eq!(
        after.sequence_number,
        before.sequence_number.wrapping_add(1)
    );
    assert!(after.timestamp.wrapping_sub(before.timestamp) > 0);
}

#[test]
fn recovery_retains_same_incarnation_repairs_but_replacement_fences_them() {
    for recover in [true, false] {
        let mut fixture = PeerFixture::connected_video();
        fixture.command(Command::SetSenderAssignment {
            sender: fixture.sender,
            assignment: Some({
                let (selected_bitrate, potential_bitrate) = (
                    MediaPayloadBitrate::from_bps(1_000_000),
                    MediaPayloadBitrate::from_bps(1_000_000),
                );
                pulsebeam_rtc::SenderAssignment {
                    selected_bitrate,
                    potential_bitrate,
                    ..fixture.assignment(fixture.sender)
                }
            }),
        });
        let warmup = fixture.send_fragmented_video_source();
        admit_video_frame(&mut fixture, &warmup, 76);
        fixture.receive_video_frame();
        fixture.drive_for(Duration::from_millis(200));
        let committed_before = fixture
            .connection
            .stats()
            .senders
            .first()
            .expect("video sender")
            .transmitted_packets;
        let sources = fixture.send_fragmented_video_source();
        fixture.drive_for(Duration::from_millis(1));
        fixture.take_video_wire_packets();
        fixture.configure_network(
            7,
            NetworkPolicy {
                delay: Duration::from_millis(100),
                ..NetworkPolicy::default()
            },
        );
        fixture.configure_impairment(RtpImpairment::Burst {
            every: 3,
            length: 1,
        });
        admit_video_frame(&mut fixture, &sources, 77);
        let deadline = fixture
            .at()
            .monotonic
            .checked_add(Duration::from_millis(150))
            .expect("bounded commit deadline");
        while fixture
            .connection
            .stats()
            .senders
            .first()
            .expect("video sender")
            .queued_packets
            > 0
        {
            assert!(fixture.at().monotonic < deadline, "bounded frame commit");
            fixture.drive_for(Duration::from_millis(1));
        }
        assert_eq!(
            fixture
                .connection
                .stats()
                .senders
                .first()
                .expect("video sender")
                .transmitted_packets,
            committed_before
                .saturating_add(u64::try_from(sources.len()).expect("bounded frame packet count"))
        );
        let primary_ssrc = fixture.receive_video_wire_header().ssrc;
        assert!(fixture.impairment_counts().1 > 0);
        fixture.command(if recover {
            Command::RecoverSenderMedia {
                sender: fixture.sender,
                assignment: fixture.assignment(fixture.sender),
            }
        } else {
            let mut assignment = fixture.assignment(fixture.sender);
            assignment.source = pulsebeam_rtc::SourceIncarnation::from_value(2);
            Command::SetSenderAssignment {
                sender: fixture.sender,
                assignment: Some(assignment),
            }
        });
        fixture.configure_impairment(RtpImpairment::Random { per_mille: 0 });
        fixture.drive_for(Duration::from_millis(700));
        let repaired = fixture
            .take_video_wire_packets()
            .iter()
            .any(|header| !header.padding && header.ssrc != primary_ssrc);
        assert_eq!(
            repaired, recover,
            "only same-publication recovery retains RTX"
        );
        if recover {
            fixture.receive_video_frame();
        }
    }
}

fn admit_video_frame(fixture: &mut PeerFixture, sources: &[pulsebeam_rtc::MediaPacket], id: u64) {
    for (index, source) in sources.iter().enumerate() {
        let mut media = forwarded(source.clone(), id);
        media.frame.boundary = if index == 0 {
            FrameBoundary::Start
        } else if index.saturating_add(1) == sources.len() {
            FrameBoundary::End
        } else {
            FrameBoundary::Middle
        };
        fixture.admit_media(media).expect("fragment admission");
    }
}

#[test]
fn assignment_rejects_unknown_sender_without_changing_existing_queue() {
    let mut fixture = PeerFixture::connected();
    let other = PeerFixture::connected_with_senders(2);
    let missing = *other.senders.get(1).expect("second negotiated sender");
    let source = fixture.send_source(b"retained source");
    fixture.command(Command::SendMedia {
        sender: fixture.sender,
        media: forwarded(source, 1),
    });
    assert!(
        matches!(fixture.connection.command(fixture.at(), Command::SetSenderAssignment {sender: missing,assignment:None}), Err(CommandError::UnknownSender(id)) if id == missing)
    );
    assert_eq!(
        fixture
            .connection
            .stats()
            .senders
            .first()
            .expect("sender")
            .queued_packets,
        1
    );
    let (_, payload) = fixture.receive_egress();
    assert_eq!(payload, b"retained source");
}
