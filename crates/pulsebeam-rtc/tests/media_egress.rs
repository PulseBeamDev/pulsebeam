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
    Command, CommandError, ConnectionConfig, ForwardedMedia, FrameBoundary, FrameDependencies,
    FrameId, FrameMetadata, MediaPayloadBitrate,
};
use support::{PeerFixture, second_negotiated_sender};

#[test]
fn public_send_media_reaches_a_standards_peer_with_stable_continuity() {
    let mut fixture = PeerFixture::connected();
    let local_source = fixture.send_source(b"local source");
    let mut other_source = PeerFixture::connected();
    let remote_source = other_source.send_source(b"remote source");
    let (first, first_expected, second, second_expected) =
        if local_source.global_media_at() <= remote_source.global_media_at() {
            (
                local_source,
                b"local source".as_slice(),
                remote_source,
                b"remote source".as_slice(),
            )
        } else {
            (
                remote_source,
                b"remote source".as_slice(),
                local_source,
                b"local source".as_slice(),
            )
        };
    let first_global = first.global_media_at();
    let second_global = second.global_media_at();

    fixture
        .connection
        .command(
            fixture.at(),
            Command::SendMedia {
                sender: fixture.sender,
                media: forwarded(first, 1),
            },
        )
        .expect("first packet admitted");
    let (first_header, first_payload) = fixture.receive_egress();
    fixture
        .connection
        .command(
            fixture.at(),
            Command::SendMedia {
                sender: fixture.sender,
                media: forwarded(second, 2),
            },
        )
        .expect("second packet admitted");
    let (second_header, second_payload) = fixture.receive_egress();

    assert_eq!(first_payload, first_expected);
    assert_eq!(second_payload, second_expected);
    assert_eq!(first_header.ssrc, second_header.ssrc);
    assert_eq!(
        second_header.sequence_number,
        first_header.sequence_number.wrapping_add(1)
    );
    assert_eq!(
        first_header
            .ext_vals
            .mid
            .as_ref()
            .map(ToString::to_string)
            .as_deref(),
        Some(fixture.sender_mid.as_str())
    );
    assert_eq!(
        second_header.ext_vals.transport_cc,
        first_header
            .ext_vals
            .transport_cc
            .map(|value| value.wrapping_add(1))
    );
    let elapsed = second_global.as_micros() - first_global.as_micros();
    let expected_ticks = u32::try_from((u128::from(elapsed) * 48_000 + 500_000) / 1_000_000)
        .expect("short fixture interval");
    assert_eq!(
        second_header.timestamp,
        first_header.timestamp.wrapping_add(expected_ticks)
    );
}

#[test]
fn public_fragmented_frame_uses_one_timestamp_and_continuous_sequences() {
    let mut fixture = PeerFixture::connected();
    let source = fixture.send_source(b"fragment");
    for boundary in [
        FrameBoundary::Start,
        FrameBoundary::Middle,
        FrameBoundary::End,
    ] {
        let mut media = forwarded(source.clone(), 77);
        media.frame.boundary = boundary;
        fixture
            .connection
            .command(
                fixture.at(),
                Command::SendMedia {
                    sender: fixture.sender,
                    media,
                },
            )
            .expect("fragment admitted");
    }
    let (first, _) = fixture.receive_egress();
    let (middle, _) = fixture.receive_egress();
    let (last, _) = fixture.receive_egress();
    assert_eq!(first.timestamp, middle.timestamp);
    assert_eq!(middle.timestamp, last.timestamp);
    assert_eq!(
        middle.sequence_number,
        first.sequence_number.wrapping_add(1)
    );
    assert_eq!(last.sequence_number, middle.sequence_number.wrapping_add(1));
}

#[test]
fn authenticated_feedback_keeps_controller_governed_egress_live() {
    let mut fixture = PeerFixture::connected();
    let first = fixture.send_source(b"first");
    fixture
        .connection
        .command(
            fixture.at(),
            Command::SendMedia {
                sender: fixture.sender,
                media: forwarded(first, 1),
            },
        )
        .expect("first media admitted");
    assert_eq!(fixture.receive_egress().1, b"first");
    fixture.drive_for(Duration::from_millis(250));
    assert!(fixture.twcc_sent() > 0, "standards peer returned TWCC");

    let second = fixture.send_source(b"second");
    fixture
        .connection
        .command(
            fixture.at(),
            Command::SendMedia {
                sender: fixture.sender,
                media: forwarded(second, 2),
            },
        )
        .expect("feedback-governed media admitted");
    assert_eq!(fixture.receive_egress().1, b"second");
}

#[test]
fn public_command_rejects_invalid_work_without_mutating_sender_continuity() {
    let mut fixture = PeerFixture::connected();
    let source = fixture.send_source(b"payload");
    let unknown = second_negotiated_sender();
    let mut invalid = forwarded(source.clone(), 1);
    invalid.frame.boundary = FrameBoundary::Middle;
    assert_eq!(
        fixture.connection.command(
            fixture.at(),
            Command::SendMedia {
                sender: fixture.sender,
                media: invalid,
            },
        ),
        Err(CommandError::InvalidFrameMetadata)
    );
    assert_eq!(
        fixture.connection.command(
            fixture.at(),
            Command::SendMedia {
                sender: unknown,
                media: forwarded(source.clone(), 2),
            },
        ),
        Err(CommandError::UnknownSender(unknown))
    );
    let mut policy = ConnectionConfig::default().default_audio_policy;
    policy.desired_bitrate = MediaPayloadBitrate::from_bps(1_000_000);
    assert_eq!(
        fixture.connection.command(
            fixture.at(),
            Command::SetSenderPolicy {
                sender: fixture.sender,
                policy,
            },
        ),
        Ok(())
    );

    fixture
        .connection
        .command(
            fixture.at(),
            Command::SendMedia {
                sender: fixture.sender,
                media: forwarded(source, 3),
            },
        )
        .expect("valid media remains admissible");
    assert_eq!(fixture.receive_egress().1, b"payload");
}

#[test]
fn public_send_media_reports_invalid_and_closed_transport_states() {
    let mut source = PeerFixture::connected();
    let packet = source.send_source(b"payload");
    let mut fixture = PeerFixture::unconnected();
    assert_eq!(
        fixture.connection.command(
            fixture.at(),
            Command::SendMedia {
                sender: fixture.sender,
                media: forwarded(packet.clone(), 1),
            },
        ),
        Err(CommandError::InvalidState)
    );

    fixture.expire_connection();
    assert_eq!(
        fixture.connection.command(
            fixture.at(),
            Command::SendMedia {
                sender: fixture.sender,
                media: forwarded(packet, 2),
            },
        ),
        Err(CommandError::Closed)
    );
}

#[test]
fn public_send_media_uses_passive_ice_tcp_framing() {
    let mut fixture = PeerFixture::connected_tcp();
    let source = fixture.send_source(b"tcp payload");
    fixture
        .connection
        .command(
            fixture.at(),
            Command::SendMedia {
                sender: fixture.sender,
                media: forwarded(source, 1),
            },
        )
        .expect("TCP media admitted");
    assert_eq!(fixture.receive_egress().1, b"tcp payload");
}

#[test]
fn public_media_admission_enforces_exact_configured_payload_bound() {
    let mut source = PeerFixture::connected();
    let packet = source.send_source(b"x");

    let mut bytes = PeerFixture::connected_with_media_limit(2);
    for id in 1..=2 {
        assert_eq!(
            bytes.connection.command(
                bytes.at(),
                Command::SendMedia {
                    sender: bytes.sender,
                    media: forwarded(packet.clone(), id),
                },
            ),
            Ok(())
        );
    }
    assert_eq!(
        bytes.connection.command(
            bytes.at(),
            Command::SendMedia {
                sender: bytes.sender,
                media: forwarded(packet, 3),
            },
        ),
        Err(CommandError::WouldBlock)
    );
}

fn forwarded(packet: pulsebeam_rtc::MediaPacket, id: u64) -> ForwardedMedia {
    ForwardedMedia {
        packet,
        frame: FrameMetadata {
            id: FrameId::from_value(id),
            boundary: FrameBoundary::Complete,
            random_access: true,
            discardable: false,
            dependencies: FrameDependencies::Known(Arc::from([])),
        },
    }
}
