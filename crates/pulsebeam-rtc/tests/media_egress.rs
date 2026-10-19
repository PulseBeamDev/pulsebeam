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
use support::{PeerFixture, second_negotiated_sender};

#[test]
fn default_peer_fixtures_share_a_global_clock_origin() {
    let first = PeerFixture::unconnected().at();
    let second = PeerFixture::unconnected().at();
    assert_eq!(first.monotonic, second.monotonic);
    assert_eq!(first.global, second.global);

    let connected = PeerFixture::connected().at();
    let elapsed = u64::try_from(
        connected
            .monotonic
            .duration_since(first.monotonic)
            .as_micros(),
    )
    .expect("short fixture interval");
    assert_eq!(
        connected.global.as_micros(),
        first.global.as_micros() + elapsed
    );
}

#[test]
fn live_peer_fixture_clock_follows_the_shared_native_world() {
    let mut first = PeerFixture::connected();
    let mut second = PeerFixture::connected();
    let before = first.at();
    assert_eq!(before.monotonic, second.at().monotonic);
    assert_eq!(before.global, second.at().global);

    second.drive_for(Duration::from_millis(20));
    let after = first.at();
    assert!(after.monotonic > before.monotonic);
    assert_eq!(after.monotonic, second.at().monotonic);
    assert_eq!(after.global, second.at().global);
    assert_eq!(
        after.global.as_micros() - before.global.as_micros(),
        u64::try_from(after.monotonic.duration_since(before.monotonic).as_micros())
            .expect("short shared clock advance")
    );

    first.drive_for(Duration::from_millis(20));
    assert!(first.at().monotonic >= after.monotonic + Duration::from_millis(20));
    assert_eq!(first.at().monotonic, second.at().monotonic);
    assert_eq!(first.at().global, second.at().global);
}

#[test]
fn public_send_media_reaches_a_standards_peer_with_stable_continuity() {
    let mut fixture = PeerFixture::connected();
    let mut other_source = PeerFixture::connected();
    let local_source = fixture.send_source(b"local source");
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
        Err(CommandError::FrameInterrupted)
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
    let mut policy = support::sender_assignment(pulsebeam_rtc::MediaPayloadBitrate::from_bps(0));
    policy.selected_bitrate = MediaPayloadBitrate::from_bps(1_000_000);
    policy.potential_bitrate = MediaPayloadBitrate::from_bps(1_000_000);
    assert_eq!(
        fixture.connection.command(
            fixture.at(),
            Command::SetSenderAssignment {
                sender: fixture.sender,
                assignment: Some(policy)
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
fn audio_talkspurts_are_native_owned_and_rearmed_by_recovery() {
    let mut fixture = PeerFixture::connected();
    let _ = fixture.send_source(b"publisher primer");
    let first_source = fixture.send_source(b"first selected audio");
    assert!(
        first_source
            .bytes()
            .get(1)
            .is_some_and(|byte| byte & 0x80 == 0)
    );
    let original = first_source.bytes().clone();
    let global = first_source.global_media_at();
    fixture.command(Command::SendMedia {
        sender: fixture.sender,
        media: forwarded(first_source.clone(), 0),
    });
    let (first, payload) = fixture.receive_egress();
    assert!(first.marker, "first selected packet starts a talk spurt");
    assert_eq!(payload, b"first selected audio");
    assert_eq!(first_source.bytes(), &original);
    assert_eq!(first_source.global_media_at(), global);
    let continuous = fixture.send_source(b"same source");
    assert!(
        continuous
            .bytes()
            .get(1)
            .is_some_and(|byte| byte & 0x80 == 0)
    );
    fixture.command(Command::SendMedia {
        sender: fixture.sender,
        media: forwarded(continuous, 1),
    });
    let (second, _) = fixture.receive_egress();
    assert!(
        !second.marker,
        "continuous packets do not restart the talk spurt"
    );
    fixture.command(Command::RecoverSenderMedia {
        sender: fixture.sender,
        assignment: fixture.assignment(fixture.sender),
    });
    let resumed = fixture.send_source(b"replacement or resumed source");
    assert!(resumed.bytes().get(1).is_some_and(|byte| byte & 0x80 == 0));
    let original = resumed.bytes().clone();
    let global = resumed.global_media_at();
    fixture.command(Command::SendMedia {
        sender: fixture.sender,
        media: forwarded(resumed.clone(), 2),
    });
    let (third, payload) = fixture.receive_egress();
    assert!(third.marker, "sender discard rearms the talk spurt");
    assert_eq!(payload, b"replacement or resumed source");
    assert_eq!(third.ssrc, first.ssrc);
    assert_eq!(
        third.sequence_number,
        second.sequence_number.wrapping_add(1)
    );
    assert_eq!(resumed.bytes(), &original);
    assert_eq!(resumed.global_media_at(), global);
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
fn public_media_admission_enforces_exact_configured_payload_bound() {
    let mut source = PeerFixture::connected();
    let mut bytes = PeerFixture::connected_with_media_limit(2);
    let packet = source.send_source(&[0xf8]);
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

#[test]
fn lost_frame_end_has_explicit_recovery_without_resetting_committed_identity() {
    let mut fixture = PeerFixture::connected();
    let source = fixture.send_source(b"interrupted frame");
    let first_global = source.global_media_at();
    let mut start = forwarded(source.clone(), 1);
    start.frame.boundary = FrameBoundary::Start;
    fixture.command(Command::SendMedia {
        sender: fixture.sender,
        media: start,
    });
    let (first, _) = fixture.receive_egress();
    assert_eq!(fixture.connection.stats().senders[0].queued_packets, 0);

    let mut duplicate = forwarded(source, 1);
    duplicate.frame.boundary = FrameBoundary::Start;
    assert_eq!(
        fixture.try_command(Command::SendMedia {
            sender: fixture.sender,
            media: duplicate,
        }),
        Err(CommandError::InvalidFrameMetadata)
    );
    let fresh_packet = fixture.send_source(b"fresh independent frame");
    let global = fresh_packet.global_media_at();
    let mut fresh = forwarded(fresh_packet.clone(), 2);
    assert_eq!(
        fixture.try_command(Command::SendMedia {
            sender: fixture.sender,
            media: fresh.clone(),
        }),
        Err(CommandError::FrameInterrupted)
    );
    let mut malformed = fresh.clone();
    malformed.frame.dependencies = FrameDependencies::Known(Arc::from([FrameId::from_value(2)]));
    assert_eq!(
        fixture.try_command(Command::SendMedia {
            sender: fixture.sender,
            media: malformed,
        }),
        Err(CommandError::InvalidFrameMetadata)
    );
    assert_eq!(fixture.connection.stats().senders[0].queued_packets, 0);

    let mut assignment = fixture.assignment(fixture.sender);
    assignment.source = pulsebeam_rtc::SourceIncarnation::from_value(2);
    fixture.command(Command::SetSenderAssignment {
        sender: fixture.sender,
        assignment: Some(assignment),
    });
    fresh.source = assignment.source;
    let mut dependent = fresh.clone();
    dependent.frame.random_access = false;
    dependent.frame.dependencies = FrameDependencies::Known(Arc::from([FrameId::from_value(1)]));
    assert_eq!(
        fixture.try_command(Command::SendMedia {
            sender: fixture.sender,
            media: dependent,
        }),
        Err(CommandError::DependencyChainInterrupted)
    );
    fixture.command(Command::SendMedia {
        sender: fixture.sender,
        media: fresh,
    });
    let (last, payload) = fixture.receive_egress();
    assert_eq!(payload, b"fresh independent frame");
    assert_eq!(last.ssrc, first.ssrc);
    assert_eq!(last.sequence_number, first.sequence_number.wrapping_add(1));
    assert_eq!(fresh_packet.global_media_at(), global);
    let elapsed = global.as_micros() - first_global.as_micros();
    let ticks = u32::try_from((u128::from(elapsed) * 48_000 + 500_000) / 1_000_000)
        .expect("short fixture interval");
    assert_eq!(last.timestamp, first.timestamp.wrapping_add(ticks));
    assert_eq!(fixture.connection.stats().senders[0].transmitted_packets, 2);
}

#[test]
fn selected_target_is_synthesized_on_authenticated_wire_without_mutating_source() {
    use pulsebeam_core::dd::{DependencyDescriptorReader, TemplateDependencyStructure, URI};

    let mut fixture = PeerFixture::connected_video_dd();
    declare_video_demand(&mut fixture);
    let mut assignment = fixture.assignment(fixture.sender);
    assignment.decode_target_mask = Some(1);
    fixture.command(Command::SetSenderAssignment {
        sender: fixture.sender,
        assignment: Some(assignment),
    });
    let mut source_reader = DependencyDescriptorReader::new();
    let mut source_structure: Option<Arc<TemplateDependencyStructure>> = None;
    let mut source_frame_ids = std::collections::BTreeMap::new();
    let mut reader = DependencyDescriptorReader::new();
    for id in 0..=4 {
        fixture.drive_for(Duration::from_millis(33));
        let temporal_index = [0, 2, 1, 2, 0][usize::try_from(id).expect("small frame index")];
        let payload = if id == 0 {
            b"\x65opaque frame"
        } else {
            b"\x41opaque frame"
        };
        let source = fixture.send_video_temporal_source(payload, Some(temporal_index));
        let descriptor = source_reader
            .read(source.extension(URI).expect("native source DD"))
            .expect("valid native source descriptor");
        if let Some(structure) = &descriptor.attached_structure {
            source_structure = Some(Arc::new(structure.as_ref().clone()));
        }
        let reference = |diff: u16| {
            *source_frame_ids
                .get(&descriptor.frame_number.wrapping_sub(diff))
                .expect("caller resolved source predecessor")
        };
        let dependencies = FrameDependencies::known(
            descriptor
                .frame_dependencies
                .frame_diffs
                .iter()
                .map(|diff| reference(*diff))
                .collect::<Vec<_>>(),
        )
        .expect("bounded source references");
        let source_facts = Arc::new(pulsebeam_rtc::SourceDescriptor {
            template_id: descriptor.template_id,
            decode_target_indications: descriptor.frame_dependencies.dtis.iter().copied().collect(),
            chain_dependencies: descriptor
                .frame_dependencies
                .chain_diffs
                .iter()
                .map(|diff| (*diff != 0).then(|| reference(u16::from(*diff))))
                .collect(),
            structure: source_structure
                .clone()
                .expect("caller resolved source structure"),
            active_decode_targets: source_reader.active_decode_targets(),
            attached_structure: descriptor.attached_structure.is_some(),
        });
        source_frame_ids.insert(descriptor.frame_number, FrameId::from_value(id));
        assert_eq!(source_facts.active_decode_targets, 15);
        if id != 0 && id != 4 {
            continue;
        }
        let original = source.bytes().clone();
        let global = source.global_media_at();
        let mut media = forwarded(source.clone(), id);
        media.frame.descriptor = Some(source_facts.clone());
        media.frame.random_access = id == 0;
        media.frame.dependencies = dependencies;
        let media = media.to_transit();
        assert_eq!(media.source, assignment.source);
        fixture
            .admit_media(media)
            .expect("selected target admission");
        let header = fixture.receive_video_wire_header();
        let received = fixture.receive_video_frame();
        assert!(
            received.data.ends_with(&payload[1..]),
            "received={:?}, expected={payload:?}",
            received.data
        );
        let raw = header.ext_vals.dependency_descriptor.expect("wire DD");
        let emitted = reader.read(&raw).expect("valid outbound descriptor");
        assert_eq!(reader.active_decode_targets(), 0b001);
        assert_eq!(emitted.frame_number, if id == 0 { 0 } else { 1 });
        assert_eq!(
            emitted.frame_dependencies.dtis,
            descriptor.frame_dependencies.dtis
        );
        if id == 0 {
            assert!(emitted.attached_structure.is_some());
            assert_eq!(emitted.active_decode_targets, Some(1));
        } else {
            assert_eq!(
                emitted.active_decode_targets, None,
                "source mask 15 must not leak back"
            );
            assert_eq!(emitted.frame_dependencies.frame_diffs.as_slice(), &[1]);
            assert_eq!(emitted.frame_dependencies.chain_diffs.as_slice(), &[1]);
        }
        assert_eq!(source.bytes(), &original);
        assert_eq!(source.global_media_at(), global);
        assert_eq!(source_facts.active_decode_targets, 15);
    }
}

#[test]
fn invalid_decode_target_masks_reject_before_committing_continuity() {
    let mut fixture = PeerFixture::connected_video_dd();
    declare_video_demand(&mut fixture);
    let source = fixture.send_video_temporal_source(b"\x65frame", Some(0));
    let mut reader = pulsebeam_core::dd::DependencyDescriptorReader::new();
    let dd = reader
        .read(
            source
                .extension(pulsebeam_core::dd::URI)
                .expect("native DD"),
        )
        .unwrap();
    let facts = pulsebeam_rtc::SourceDescriptor {
        template_id: dd.template_id,
        decode_target_indications: dd.frame_dependencies.dtis.into_iter().collect(),
        chain_dependencies: dd
            .frame_dependencies
            .chain_diffs
            .into_iter()
            .map(|diff| {
                assert_eq!(diff, 0);
                None
            })
            .collect(),
        structure: Arc::new(reader.structure().unwrap().clone()),
        active_decode_targets: reader.active_decode_targets(),
        attached_structure: dd.attached_structure.is_some(),
    };
    let mut media = forwarded(source, 0);
    media.frame.descriptor = Some(Arc::new(facts));
    for mask in [0, 16, u32::MAX] {
        let mut assignment = fixture.assignment(fixture.sender);
        assignment.decode_target_mask = Some(mask);
        let updated = fixture.try_command(Command::SetSenderAssignment {
            sender: fixture.sender,
            assignment: Some(assignment),
        });
        if mask == 0 {
            assert_eq!(updated, Err(CommandError::InvalidSenderAssignment));
        } else {
            updated.expect("nonzero selection requires source validation");
            assert_eq!(
                fixture.try_command(Command::SendMedia {
                    sender: fixture.sender,
                    media: media.clone()
                }),
                Err(CommandError::InvalidFrameMetadata)
            );
        }
        assert_eq!(fixture.connection.stats().senders[0].queued_packets, 0);
    }
    let mut assignment = fixture.assignment(fixture.sender);
    assignment.decode_target_mask = Some(1);
    fixture.command(Command::SetSenderAssignment {
        sender: fixture.sender,
        assignment: Some(assignment),
    });
    fixture.command(Command::SendMedia {
        sender: fixture.sender,
        media,
    });
    let _ = fixture.receive_video_wire_header();
    let received = fixture.receive_video_frame();
    assert!(
        received.data.ends_with(b"frame"),
        "received={:?}",
        received.data
    );
}

fn declare_video_demand(fixture: &mut PeerFixture) {
    let mut policy = fixture
        .connection
        .stats()
        .senders
        .first()
        .expect("video sender")
        .assignment
        .expect("bound sender");
    policy.selected_bitrate = MediaPayloadBitrate::from_bps(1_000_000);
    policy.potential_bitrate = MediaPayloadBitrate::from_bps(1_000_000);
    fixture.command(Command::SetSenderAssignment {
        sender: fixture.sender,
        assignment: Some(policy),
    });
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
