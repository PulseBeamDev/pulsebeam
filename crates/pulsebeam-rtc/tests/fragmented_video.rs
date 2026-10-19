#![allow(
    clippy::arithmetic_side_effects,
    clippy::disallowed_types,
    clippy::expect_used,
    reason = "public integration tests construct reviewed immutable packet metadata"
)]

mod support;

use std::sync::Arc;

use pulsebeam_rtc::{
    Command, CommandError, ForwardedMedia, FrameBoundary, FrameDependencies, FrameId,
    FrameMetadata, MediaPayloadBitrate,
};
use support::PeerFixture;

fn source_sequence(source: &pulsebeam_rtc::MediaPacket) -> u16 {
    u16::from_be_bytes(
        source
            .bytes()
            .get(2..4)
            .expect("source sequence field")
            .try_into()
            .expect("two sequence bytes"),
    )
}

#[test]
fn public_fragmented_frame_uses_one_timestamp_and_continuous_sequences() {
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
    let sources = fixture.send_fragmented_video_source();
    assert!(sources.len() >= 3);
    let global = sources.first().expect("source start").global_media_at();
    for pair in sources.windows(2) {
        assert_eq!(
            source_sequence(&pair[1]),
            source_sequence(&pair[0]).wrapping_add(1)
        );
    }
    let mut headers = Vec::new();
    for (index, source) in sources.iter().enumerate() {
        assert_eq!(source.global_media_at(), global);
        let mut media = forwarded(source.clone(), 77);
        media.frame.boundary = if index == 0 {
            FrameBoundary::Start
        } else if index.saturating_add(1) == sources.len() {
            FrameBoundary::End
        } else {
            FrameBoundary::Middle
        };
        admit_fragment(&mut fixture, media);
    }
    for _ in &sources {
        headers.push(fixture.receive_video_wire_header());
    }
    for pair in headers.windows(2) {
        assert_eq!(pair[0].timestamp, pair[1].timestamp);
        assert_eq!(
            pair[1].sequence_number,
            pair[0].sequence_number.wrapping_add(1)
        );
    }
    assert!(headers.first().is_some_and(|header| !header.marker));
    assert!(headers.last().is_some_and(|header| header.marker));
}

#[test]
fn source_middle_loss_interrupts_native_frame_without_committing_a_false_end() {
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
    let sources = fixture.send_fragmented_video_source();
    assert!(sources.len() >= 3);
    let first = sources.first().expect("source start");
    let end = sources.last().expect("source end");
    assert!(source_sequence(end).wrapping_sub(source_sequence(first)) > 1);
    assert_eq!(first.global_media_at(), end.global_media_at());
    let original = end.bytes().clone();
    let global = end.global_media_at();
    let mut start = forwarded(first.clone(), 77);
    start.frame.boundary = FrameBoundary::Start;
    fixture.admit_media(start).expect("source start admission");
    let committed = fixture.receive_video_wire_header();
    assert!(!committed.marker);
    let counters = fixture.connection.stats();
    let mut lost_middle = forwarded(end.clone(), 77);
    lost_middle.frame.boundary = FrameBoundary::End;
    assert_eq!(
        fixture.connection.command(
            fixture.at(),
            Command::SendMedia {
                sender: fixture.sender,
                media: lost_middle
            }
        ),
        Err(CommandError::FrameInterrupted)
    );
    let rejected = fixture.connection.stats();
    assert_eq!(
        rejected.senders[0].transmitted_packets,
        counters.senders[0].transmitted_packets
    );
    assert_eq!(rejected.senders[0].queued_packets, 0);
    assert_eq!(end.bytes(), &original);
    assert_eq!(end.global_media_at(), global);
    fixture.command(Command::RecoverSenderMedia {
        sender: fixture.sender,
        assignment: fixture.assignment(fixture.sender),
    });
    let recovery = fixture.send_source(&[0x65, 0x55]);
    fixture
        .admit_media(forwarded(recovery, 78))
        .expect("source recovery admission");
    let mut sequence = committed.sequence_number;
    let resumed = loop {
        let packet = fixture.receive_video_wire_packet();
        assert_eq!(packet.ssrc, committed.ssrc);
        assert_eq!(packet.sequence_number, sequence.wrapping_add(1));
        sequence = packet.sequence_number;
        if !packet.padding {
            break packet;
        }
    };
    assert!(resumed.marker);
    assert!(resumed.timestamp > committed.timestamp);
}

#[test]
fn frame_metadata_must_match_all_fragments_without_wire_dd() {
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
                decode_target_mask: Some(1),
                ..fixture.assignment(fixture.sender)
            }
        }),
    });
    let sources = fixture.send_fragmented_video_source();
    for (index, source) in sources.iter().enumerate() {
        let mut media = forwarded(source.clone(), 77);
        media.frame.boundary = if index == 0 {
            FrameBoundary::Start
        } else if index + 1 == sources.len() {
            FrameBoundary::End
        } else {
            FrameBoundary::Middle
        };
        if index == 1 {
            let before = fixture.connection.stats();
            let original = source.bytes().clone();
            let mut changed = media.clone();
            changed.frame.random_access = false;
            assert_eq!(
                fixture.try_command(Command::SendMedia {
                    sender: fixture.sender,
                    media: changed
                }),
                Err(CommandError::InvalidFrameMetadata),
            );
            let after = fixture.connection.stats();
            assert_eq!(
                after.senders[0].queued_packets,
                before.senders[0].queued_packets
            );
            assert_eq!(
                after.senders[0].transmitted_packets,
                before.senders[0].transmitted_packets
            );
            assert_eq!(source.bytes(), &original);
        }
        admit_fragment(&mut fixture, media);
    }
    let headers: Vec<_> = sources
        .iter()
        .map(|_| fixture.receive_video_wire_header())
        .collect();
    for pair in headers.windows(2) {
        assert_eq!(
            pair[1].sequence_number,
            pair[0].sequence_number.wrapping_add(1)
        );
        assert_eq!(pair[1].timestamp, pair[0].timestamp);
    }
    assert!(!headers[0].marker);
    assert!(headers.last().expect("frame end").marker);
}

fn admit_fragment(fixture: &mut PeerFixture, media: ForwardedMedia) {
    fixture.admit_media(media).expect("fragment admission");
}

fn forwarded(packet: pulsebeam_rtc::MediaPacket, id: u64) -> ForwardedMedia {
    ForwardedMedia {
        source: pulsebeam_rtc::SourceIncarnation::from_value(1),
        packet,
        frame: FrameMetadata {
            descriptor: None,
            id: FrameId::from_value(id),
            boundary: FrameBoundary::Complete,
            random_access: true,
            discardable: false,
            dependencies: FrameDependencies::Known(Arc::from([])),
        },
    }
}
