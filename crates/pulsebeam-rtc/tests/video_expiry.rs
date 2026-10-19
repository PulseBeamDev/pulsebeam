#![allow(
    clippy::arithmetic_side_effects,
    clippy::disallowed_types,
    clippy::expect_used,
    reason = "public integration tests construct reviewed immutable packet metadata"
)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use pulsebeam_rtc::{
    Command, CommandError, ForwardedMedia, FrameBoundary, FrameDependencies, FrameId,
    FrameMetadata, MediaPayloadBitrate,
};
use support::PeerFixture;

#[test]
fn video_frame_expiry_requests_recovery_before_idle_and_preserves_continuity() {
    use pulsebeam_rtc::{Event, Output};

    let mut fixture = PeerFixture::connected_video();
    let mut policy = fixture.connection.stats().senders[0]
        .assignment
        .expect("bound sender");
    policy.selected_bitrate = MediaPayloadBitrate::from_bps(1_000_000);
    policy.potential_bitrate = MediaPayloadBitrate::from_bps(1_000_000);
    fixture.command(Command::SetSenderAssignment {
        sender: fixture.sender,
        assignment: Some(policy),
    });
    let seed = fixture.send_source(b"\x65seed");
    fixture
        .admit_media(forwarded(seed, 0))
        .expect("admit independent seed before expiry");
    let first = fixture.receive_video_wire_header();
    assert!(fixture.receive_video_frame().data.ends_with(b"seed"));
    fixture.drive_for(Duration::from_millis(33));
    let partial = fixture.send_source(b"\x61partial");
    let mut interrupted = forwarded(partial, 1);
    interrupted.frame.boundary = FrameBoundary::Start;
    fixture
        .admit_media(interrupted)
        .expect("admit incomplete frame before expiry");
    fixture.advance_clock_without_poll(Duration::from_secs(1));
    let mut requested = false;
    for _ in 0..32 {
        match fixture.connection.poll(fixture.at()) {
            Output::Event(Event::KeyframeRequested { sender, reason }) => {
                assert_eq!(sender, fixture.sender);
                assert_eq!(reason, pulsebeam_rtc::KeyframeRequestReason::LocalRecovery);
                requested = true;
                break;
            }
            Output::Idle { .. } => panic!("expiry recovery event must not be hidden behind idle"),
            Output::Closed(reason) => panic!("unexpected closure: {reason:?}"),
            _ => {}
        }
    }
    assert!(
        requested,
        "expiry must notify the caller of lost dependency continuity"
    );
    assert_eq!(fixture.connection.stats().senders[0].queued_packets, 0);
    let fresh = fixture.send_source(b"\x65fresh independent frame");
    let original = fresh.bytes().clone();
    let global = fresh.global_media_at();
    let mut dependent = forwarded(fresh.clone(), 2);
    dependent.frame.random_access = false;
    dependent.frame.dependencies = FrameDependencies::Known(Arc::from([FrameId::from_value(1)]));
    assert_eq!(
        fixture
            .try_command(Command::SendMedia {
                sender: fixture.sender,
                media: dependent
            })
            .map_err(|rejected| rejected.reason),
        Err(CommandError::DependencyChainInterrupted)
    );
    fixture
        .admit_media(forwarded(fresh.clone(), 3))
        .expect("admit independent recovery after expiry");
    let mut sequence = first.sequence_number;
    let last = loop {
        let packet = fixture.receive_video_wire_packet();
        assert_eq!(packet.ssrc, first.ssrc);
        assert_eq!(packet.sequence_number, sequence.wrapping_add(1));
        sequence = packet.sequence_number;
        if !packet.padding {
            break packet;
        }
    };
    let received = fixture.receive_video_frame();
    assert_eq!(received.rtp_timestamp, last.timestamp);
    assert!(received.data.ends_with(b"fresh independent frame"));
    assert_eq!(fresh.bytes(), &original);
    assert_eq!(fresh.global_media_at(), global);
    fixture.drive_for(Duration::from_millis(33));
    let mut resumed = forwarded(fixture.send_source(b"\x61dependent after recovery"), 4);
    resumed.frame.random_access = false;
    resumed.frame.dependencies = FrameDependencies::Known(Arc::from([FrameId::from_value(3)]));
    let mut admitted = false;
    for _ in 0..50 {
        match fixture.try_command(Command::SendMedia {
            sender: fixture.sender,
            media: resumed.clone(),
        }) {
            Ok(()) => {
                admitted = true;
                break;
            }
            Err(pulsebeam_rtc::RejectedCommand {
                reason: CommandError::WouldBlock,
                ..
            }) => fixture.drive_for(Duration::from_millis(1)),
            Err(error) => panic!("dependent admission after recovery: {error:?}"),
        }
    }
    assert!(
        admitted,
        "dependent admission must resume after bounded pacing wait"
    );
    let _ = fixture.receive_video_wire_header();
    assert!(
        fixture
            .receive_video_frame()
            .data
            .ends_with(b"dependent after recovery")
    );
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
