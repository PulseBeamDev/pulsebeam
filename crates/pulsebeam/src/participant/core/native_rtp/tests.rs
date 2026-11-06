use super::*;
use pulsebeam_proto::state::resource_spec;
mod receiver;
use crate::control::native::track_descriptor;
use crate::participant::core::native_data::tests::{
    NativeParticipant, confirmations, frame, participant_with_capabilities,
};
use crate::participant::event::test_utils::MockParticipantSink;

fn opus() -> wire::CodecFormat {
    wire::CodecFormat::from(wire::codec_format::Codec::Opus(
        wire::OpusParameters::default(),
    ))
}

fn capabilities() -> wire::RtpCapabilities {
    let video = crate::control::rtp::capabilities()
        .codecs
        .into_iter()
        .find(|codec| {
            matches!(
                codec
                    .format
                    .as_ref()
                    .and_then(wire::CodecFormat::known_codec),
                Some(wire::codec_format::Codec::H264(_))
            )
        })
        .unwrap();
    wire::RtpCapabilities {
        codecs: vec![
            wire::RtpCodecCapability {
                format: Some(opus()),
                feedback: vec![wire::RtcpFeedback::TransportCc.as_str().into()],
                rtx_supported: false,
                scalability_modes: vec![],
            },
            video,
        ],
        header_extensions: vec![
            wire::RtpHeaderExtensionType::RtpHeaderExtensionMid
                .as_str()
                .into(),
            wire::RtpHeaderExtensionType::RtpHeaderExtensionTransportCcDraft01
                .as_str()
                .into(),
            wire::RtpHeaderExtensionType::RtpHeaderExtensionPlayoutDelay
                .as_str()
                .into(),
            wire::RtpHeaderExtensionType::RtpHeaderExtensionDependencyDescriptor
                .as_str()
                .into(),
        ],
        rtcp_modes: vec![wire::RtcpMode::Compound.as_str().into()],
    }
}

fn spec(enabled: bool) -> wire::ResourceSpec {
    wire::ResourceSpec {
        settings: Some(wire::ResourceSettings { enabled: Some(enabled), receive: None }),
        declaration: Some(resource_spec::Declaration::Publication(wire::PublicationSpec {
            label: "microphone".into(),
            kind: Some(wire::publication_spec::Kind::Audio(wire::AudioPublication {
                binding: Some(wire::TrackBinding { transport: Some(wire::track_binding::Transport::Rtp(wire::RtpParameters {
                    mid: "mic".into(),
                    codec: Some(wire::RtpCodecParameters { payload_type: Some(111), rtx_payload_type: None, feedback: vec![wire::RtcpFeedback::TransportCc.as_str().into()], format: Some(opus()) }),
                    header_extensions: vec![wire::RtpHeaderExtensionParameters { id: 1, uri: wire::RtpHeaderExtensionType::RtpHeaderExtensionMid.as_str().into() }, wire::RtpHeaderExtensionParameters { id: 2, uri: wire::RtpHeaderExtensionType::RtpHeaderExtensionTransportCcDraft01.as_str().into() }],
                    encodings: vec![wire::RtpEncodingParameters { ssrc: Some(0), rtx_ssrc: None, rid: String::new(), scalability_mode: None }],
                    rtcp: Some(wire::RtcpParameters { cname: "component-source-cname".into(), reduced_size: false }),
                })) }),
            })),
        })),
    }
}

fn video_spec(
    mid: &str,
    ssrc: u32,
    mode: wire::RtpScalabilityMode,
    descriptor: bool,
) -> wire::ResourceSpec {
    let format = capabilities()
        .codecs
        .into_iter()
        .find_map(|codec| {
            codec.format.filter(|format| {
                matches!(
                    format.known_codec(),
                    Some(wire::codec_format::Codec::H264(_))
                )
            })
        })
        .unwrap();
    let mut header_extensions = vec![
        wire::RtpHeaderExtensionParameters {
            id: 1,
            uri: wire::RtpHeaderExtensionType::RtpHeaderExtensionMid
                .as_str()
                .into(),
        },
        wire::RtpHeaderExtensionParameters {
            id: 2,
            uri: wire::RtpHeaderExtensionType::RtpHeaderExtensionTransportCcDraft01
                .as_str()
                .into(),
        },
    ];
    if descriptor {
        header_extensions.push(wire::RtpHeaderExtensionParameters {
            id: 3,
            uri: wire::RtpHeaderExtensionType::RtpHeaderExtensionDependencyDescriptor
                .as_str()
                .into(),
        });
    }
    wire::ResourceSpec {
        settings: spec(true).settings,
        declaration: Some(resource_spec::Declaration::Publication(
            wire::PublicationSpec {
                label: mid.into(),
                kind: Some(wire::publication_spec::Kind::Video(
                    wire::VideoPublication {
                        binding: Some(wire::TrackBinding {
                            transport: Some(wire::track_binding::Transport::Rtp(
                                wire::RtpParameters {
                                    mid: mid.into(),
                                    codec: Some(wire::RtpCodecParameters {
                                        payload_type: Some(96),
                                        feedback: vec![
                                            wire::RtcpFeedback::TransportCc.as_str().into(),
                                        ],
                                        format: Some(format),
                                        ..Default::default()
                                    }),
                                    header_extensions,
                                    encodings: [ssrc, ssrc + 2, ssrc + 4]
                                        .into_iter()
                                        .map(|ssrc| wire::RtpEncodingParameters {
                                            ssrc: Some(ssrc),
                                            scalability_mode: Some(mode.into()),
                                            ..Default::default()
                                        })
                                        .collect(),
                                    rtcp: Some(wire::RtcpParameters {
                                        cname: "component-video-source".into(),
                                        reduced_size: false,
                                    }),
                                },
                            )),
                        }),
                    },
                )),
            },
        )),
    }
}

#[track_caller]
fn last_live(participant: &mut NativeParticipant) -> (wire::ConfirmedState, wire::LiveResource) {
    last_live_for(participant, 1)
}

#[track_caller]
fn last_live_for(
    participant: &mut NativeParticipant,
    reference: u64,
) -> (wire::ConfirmedState, wire::LiveResource) {
    let value = native_data::tests::confirmations_for(participant, reference)
        .pop()
        .expect("owned output");
    let Some(wire::confirmed_state::Value::Owned(live)) = value.value.as_ref() else {
        panic!("accepted owned source/receiver, got {value:?}")
    };
    (value.clone(), live.as_ref().clone())
}

#[test]
fn unsupported_congestion_control_rejects_before_source_installation() {
    for (feedback, extension) in [(false, true), (true, false), (false, false)] {
        let mut participant =
            participant_with_capabilities(native_data::unix_now() + 60, Some(capabilities()));
        let mut sink = MockParticipantSink::new();
        let mut unsupported = spec(true);
        let Some(resource_spec::Declaration::Publication(publication)) =
            unsupported.declaration.as_mut()
        else {
            panic!("publication")
        };
        let Some(wire::publication_spec::Kind::Audio(audio)) = publication.kind.as_mut() else {
            panic!("audio")
        };
        let Some(wire::track_binding::Transport::Rtp(rtp)) =
            audio.binding.as_mut().unwrap().transport.as_mut()
        else {
            panic!("RTP")
        };
        if !feedback {
            rtp.codec.as_mut().unwrap().feedback.clear();
        }
        if !extension {
            rtp.header_extensions.retain(|extension| {
                extension.uri
                    != wire::RtpHeaderExtensionType::RtpHeaderExtensionTransportCcDraft01.as_str()
            });
        }
        assert_eq!(wire::validation::validate_spec(&unsupported), Ok(()));
        participant.handle_native_signaling(
            &frame(
                1,
                wire::desired_state::Value::Present(Box::new(unsupported)),
            ),
            &mut sink,
        );
        let rejected = confirmations(&mut participant).pop().unwrap();
        assert_eq!(rejected.evaluated_revision, 1);
        assert_eq!(
            rejected.rejection.unwrap().code,
            wire::FailureCode::Unsupported as i32
        );
        assert!(participant.native_rtp.publications.is_empty());
        assert!(sink.publish_track_calls.is_empty());
        assert!(participant.native_rtp.ssrcs.is_empty());
        assert!(participant.native_rtp.mids.is_empty());
        assert!(participant.disconnect_reason.is_none());

        participant.handle_native_signaling(
            &native_data::tests::frame_for(
                2,
                2,
                wire::desired_state::Value::Present(Box::new(spec(true))),
            ),
            &mut sink,
        );
        let (accepted, live) = last_live_for(&mut participant, 2);
        assert_eq!(accepted.evaluated_revision, 2);
        assert!(accepted.rejection.is_none());
        assert_eq!(live.execution, wire::ExecutionStatus::Installed as i32);
        assert_eq!(sink.publish_track_calls.len(), 1);
    }
}

#[test]
fn admitted_source_keeps_birth_identity_demand_and_cutoff_independent_of_desire() {
    let mut participant =
        participant_with_capabilities(native_data::unix_now() + 60, Some(capabilities()));
    let mut sink = MockParticipantSink::new();
    participant.handle_native_signaling(
        &frame(1, wire::desired_state::Value::Present(Box::new(spec(true)))),
        &mut sink,
    );
    assert!(participant.disconnect_reason.is_none());
    let (value, live) = last_live(&mut participant);
    assert_eq!(value.evaluated_revision, 1);
    assert!(value.rejection.is_none());
    assert_eq!(live.execution, wire::ExecutionStatus::Installed as i32);
    assert_eq!(live.activity, Some(wire::MediaActivity::Idle as i32));
    assert_eq!(live.spec, Some(spec(true)));
    let source = participant.native_rtp.publications.get(&1).unwrap();
    let id = source.track.id();
    assert_eq!(
        id,
        participant.connection_id.derive_publication_track_id(
            TrackKind::Audio,
            "microphone",
            NonZeroU64::new(1).unwrap()
        )
    );
    assert_ne!(
        id,
        participant
            .connection_id
            .derive_track_id(TrackKind::Audio, "microphone")
    );
    assert_eq!(live.track, track_descriptor(&source.track));
    assert_eq!(
        live.track.as_ref().unwrap().kind,
        Some(wire::track::Kind::Audio(wire::AudioTrack {
            format: Some(opus())
        }))
    );
    let (_, _, compiled) =
        crate::control::rtp::policy::publication(&spec(true), &capabilities()).unwrap();
    assert_eq!(source.binding.description(), compiled.description());
    assert!(!source.binding.description().encodings[0].active);
    receiver::check(&source.track);
    receiver::check_prebound(&source.track);
    assert_eq!(sink.publish_track_calls, vec![id]);
    assert_eq!(participant.upstream.mid_for_track_id(id), None);
    assert!(participant.native_rtp.owns_encoding(0));
    let mut source_handles = slotmap::SlotMap::<TrackHandle, TrackId>::with_key();
    let source_fanout = source_handles.insert(id);
    participant.apply(
        ParticipantEffect::TrackPublished { track_id: id },
        Some(source_fanout),
    );
    assert_eq!(
        participant.upstream.track_for_fanout(source_fanout),
        Some(id)
    );

    participant.native_rtp_demand(id, true);
    let (_, live) = last_live(&mut participant);
    assert_eq!(live.activity, Some(wire::MediaActivity::Active as i32));
    assert!(
        participant.native_rtp.publications[&1]
            .binding
            .description()
            .encodings[0]
            .active
    );
    participant.handle_native_signaling(
        &frame(
            2,
            wire::desired_state::Value::Present(Box::new(spec(false))),
        ),
        &mut sink,
    );
    let (_, live) = last_live(&mut participant);
    assert_eq!(live.execution, wire::ExecutionStatus::Disabled as i32);
    assert_eq!(live.activity, Some(wire::MediaActivity::Idle as i32));
    assert!(
        !participant.native_rtp.publications[&1]
            .binding
            .description()
            .encodings[0]
            .active
    );
    assert_eq!(participant.native_rtp.publications[&1].track.id(), id);
    participant.handle_native_signaling(
        &frame(3, wire::desired_state::Value::Present(Box::new(spec(true)))),
        &mut sink,
    );
    let (_, live) = last_live(&mut participant);
    assert_eq!(live.activity, Some(wire::MediaActivity::Active as i32));
    assert!(
        participant.native_rtp.publications[&1]
            .binding
            .description()
            .encodings[0]
            .active
    );
    assert_eq!(sink.publish_track_calls, vec![id]);

    let mut incompatible = spec(true);
    let Some(resource_spec::Declaration::Publication(publication)) =
        incompatible.declaration.as_mut()
    else {
        unreachable!()
    };
    publication.label = "different-source".into();
    participant.handle_native_signaling(
        &frame(
            4,
            wire::desired_state::Value::Present(Box::new(incompatible)),
        ),
        &mut sink,
    );
    let (rejected, live) = last_live(&mut participant);
    assert!(rejected.rejection.is_some());
    assert_eq!(live.spec, Some(spec(true)));
    participant.native_rtp_demand(id, false);
    let (updated, live) = last_live(&mut participant);
    assert_eq!(updated.evaluated_revision, rejected.evaluated_revision);
    assert_eq!(updated.rejection, rejected.rejection);
    assert_eq!(live.activity, Some(wire::MediaActivity::Idle as i32));
    assert!(
        !participant.native_rtp.publications[&1]
            .binding
            .description()
            .encodings[0]
            .active
    );

    let expiry = participant.authorization_expiry;
    participant.authorization_expiry =
        Some(pulsebeam_core::auth::AuthorizationExpiry::from_unix_seconds(1));
    participant.handle_native_signaling(
        &frame(5, wire::desired_state::Value::Absent(true)),
        &mut sink,
    );
    assert!(participant.native_rtp.publications[&1].retiring);
    assert!(
        participant.native_rtp.tracks.is_empty()
            && participant.native_rtp.mids.is_empty()
            && participant.native_rtp.ssrcs.is_empty()
            && participant.native_rtp.active.is_empty()
    );
    assert_eq!(participant.upstream.track_for_fanout(source_fanout), None);
    assert_eq!(sink.unpublish_track_calls, vec![id]);
    participant.retire_native_resource(1, &mut sink);
    assert_eq!(sink.unpublish_track_calls, vec![id]);
    assert!(participant.native_rtp.publications.contains_key(&1));
    let retired_slot = participant.native_rtp.publications[&1].slot;
    assert!(participant.upstream.track_mut(retired_slot).is_some());
    participant.native_rtp_demand(id, true);
    assert!(participant.native_rtp.publications[&1].retiring);
    let retired_binding = participant.native_rtp.publications[&1]
        .binding
        .description();
    let retired_mid = Mid::new(&retired_binding.mid).unwrap();
    let retired_ssrc = retired_binding.encodings[0].primary;
    participant.native_rtp.mids.insert(retired_mid, 2);
    participant.native_rtp.ssrcs.insert(retired_ssrc, 2);
    participant.native_rtp.detach(1);
    assert_eq!(participant.native_rtp.mids.get(&retired_mid), Some(&2));
    assert_eq!(participant.native_rtp.ssrcs.get(&retired_ssrc), Some(&2));
    participant.native_rtp.mids.remove(&retired_mid);
    participant.native_rtp.ssrcs.remove(&retired_ssrc);
    participant.complete_native_resource_cutoff(1, true);
    let absent = confirmations(&mut participant).pop().unwrap();
    assert!(matches!(
        absent.value,
        Some(wire::confirmed_state::Value::Absent(_))
    ));
    assert!(participant.native_rtp.publications.is_empty());
    assert!(participant.upstream.track_mut(retired_slot).is_none());
    assert!(participant.disconnect_reason.is_none());
    participant.authorization_expiry = expiry;

    for (revision, descriptor) in [(6, false), (7, true)] {
        participant.handle_native_signaling(
            &native_data::tests::frame_for(
                revision,
                revision,
                wire::desired_state::Value::Present(Box::new(video_spec(
                    "camera",
                    77,
                    wire::RtpScalabilityMode::L1t3,
                    descriptor,
                ))),
            ),
            &mut sink,
        );
        if !descriptor {
            let rejected = native_data::tests::confirmations_for(&mut participant, 6)
                .pop()
                .unwrap();
            assert_eq!(
                rejected.rejection.unwrap().code,
                wire::FailureCode::Unsupported as i32
            );
            assert!(!participant.native_rtp.publications.contains_key(&6));
        }
    }
    let (accepted, live) = last_live_for(&mut participant, 7);
    assert!(accepted.rejection.is_none());
    assert_eq!(
        live.spec,
        Some(video_spec(
            "camera",
            77,
            wire::RtpScalabilityMode::L1t3,
            true
        ))
    );
    let video_id = participant.native_rtp.publications[&7].track.id();
    let encodings = participant.native_rtp.publications[&7]
        .track
        .layers()
        .iter()
        .map(|layer| {
            assert!(layer.rid.is_none());
            layer.encoding
        })
        .collect::<Vec<_>>();
    assert_eq!(
        encodings,
        [77, 79, 81].map(|ssrc| track::SourceEncoding::Ssrc(ssrc.into()))
    );
    let mut handles = slotmap::SlotMap::<TrackHandle, TrackId>::with_key();
    let fanout = handles.insert(video_id);
    participant.apply(
        ParticipantEffect::TrackPublished { track_id: video_id },
        Some(fanout),
    );
    assert_eq!(
        participant.upstream.track_for_fanout(fanout),
        Some(video_id)
    );
    participant.native_rtp_demand(video_id, true);
    let other_kind = TrackId::Audio(entity::AudioTrackId::from_bytes(*video_id.as_bytes()));
    participant.native_rtp_demand(other_kind, false);
    assert!(participant.native_rtp.publications[&7].demanded);
    assert!(!participant.native_rtp.bind_source(other_kind, fanout));
    assert_eq!(
        participant
            .native_rtp
            .keyframe_mid(other_kind, track::SourceEncoding::Ssrc(77.into()),),
        None
    );
    let route = participant
        .native_rtp
        .ingress_route(77, Mid::from("camera"), None)
        .unwrap();
    assert_eq!(
        route.upstream_slot,
        participant.native_rtp.publications[&7].slot
    );
    assert_eq!(route.encoding, track::SourceEncoding::Ssrc(77.into()));
    let mut cache = crate::rtp::cache::TrackStreamCache::with_encodings(&encodings);
    let now = Instant::now();
    for ssrc in [81, 77, 79] {
        // The RTC emits this notice before its first authenticated packet. Use
        // the committed binding, not bootstrap SDP or guessed source facts.
        let binding = participant.native_rtp.publications[&7]
            .binding
            .description();
        let info = pulsebeam_rtc::EncodingInfo {
            mid: binding.mid.as_str().into(),
            ssrc,
            kind: binding.kind,
            rid: None,
            payloads: Box::new([pulsebeam_rtc::RtpPayloadInfo {
                payload_type: binding.codec.payload_type,
                name: binding.codec.parameters.name.as_str().into(),
                clock_rate: binding.codec.parameters.clock_rate,
            }]),
        };
        participant.handle_event(now, Event::EncodingDiscovered(info), &mut sink);
        let published_before = sink.publish_track_packet_calls.len();
        let mut builder = crate::rtp::test_utils::H264StreamBuilder::new(ssrc, 1000, 90_000, now);
        let encoding = track::SourceEncoding::Ssrc(ssrc.into());
        assert_eq!(
            participant.native_rtp.keyframe_mid(video_id, encoding),
            Some(Mid::from("camera"))
        );
        for packet in builder.keyframe(2) {
            assert!(packet.ext_vals.rid.is_none());
            assert_eq!(cache.source(&packet), Some(encoding));
            let incoming = pulsebeam_rtc::RtpPacket {
                arrival: packet.arrival_ts.into(),
                header: pulsebeam_rtc::RtpHeader {
                    sequence: u16::try_from(*packet.seq_no).unwrap(),
                    timestamp: u32::try_from(packet.rtp_ts.numer()).unwrap(),
                    ssrc,
                    payload_type: 96,
                    marker: packet.marker,
                },
                payload: bytes::Bytes::copy_from_slice(&packet.payload),
                extensions: Vec::new(),
            };
            participant.handle_incoming_rtp(incoming, Mid::from("camera"), None, &mut sink);
            cache.push_encoding(encoding, packet);
        }
        assert!(
            sink.publish_track_packet_calls.len() > published_before,
            "source {ssrc} must publish real processed packets"
        );
        participant.enqueue_remote_keyframe((video_id, encoding), KeyframeRequestKind::Pli);
    }
    let video_slot = participant.native_rtp.publications[&7].slot;
    let source = participant.upstream.track_mut(video_slot).unwrap();
    source.poll_stats(now + std::time::Duration::from_millis(50));
    let states = source.monitor.layer_states();
    assert_eq!(states.len(), 3);
    for encoding in &encodings {
        assert!(
            !states
                .iter()
                .find(|(key, _)| key == encoding)
                .unwrap()
                .1
                .is_inactive()
        );
        let packet = cache.encoding(*encoding).unwrap().get(1000.into()).unwrap();
        assert!(encoding.matches(packet.ssrc, packet.ext_vals.rid));
        assert!(packet.ext_vals.rid.is_none());
        assert!(
            participant
                .pending_keyframe_requests
                .contains(&(Mid::from("camera"), *encoding))
        );
    }
    assert_eq!(participant.pending_keyframe_requests.len(), 3);
    assert!(cache.source(&crate::rtp::RtpPacket::default()).is_none());
    assert_eq!(
        participant
            .native_rtp
            .keyframe_mid(video_id, track::SourceEncoding::Ssrc(999.into())),
        None
    );
    assert_eq!(
        participant
            .native_rtp
            .keyframe_mid(video_id, track::SourceEncoding::Rid(None)),
        None
    );
    assert!(
        sink.publish_track_packet_calls
            .iter()
            .all(|handle| *handle == fanout)
    );
    assert!(!sink.publish_track_packet_calls.is_empty());
    let video_binding = participant.native_rtp.publications[&7]
        .binding
        .description()
        .clone();
    assert_eq!(sink.publish_track_calls, vec![id, video_id]);
    participant.handle_native_signaling(
        &native_data::tests::frame_for(
            8,
            8,
            wire::desired_state::Value::Present(Box::new(video_spec(
                "other",
                78,
                wire::RtpScalabilityMode::L1t2,
                true,
            ))),
        ),
        &mut sink,
    );
    let rejected = native_data::tests::confirmations_for(&mut participant, 8)
        .pop()
        .unwrap();
    assert_eq!(
        rejected.rejection.unwrap().code,
        wire::FailureCode::Unsupported as i32
    );
    assert!(!participant.native_rtp.publications.contains_key(&8));
    assert_eq!(participant.native_rtp.publications[&7].track.id(), video_id);
    assert_eq!(
        *participant.native_rtp.publications[&7]
            .binding
            .description(),
        video_binding
    );
    assert_eq!(sink.publish_track_calls, vec![id, video_id]);
    participant.handle_native_signaling(
        &native_data::tests::frame_for(7, 9, wire::desired_state::Value::Absent(true)),
        &mut sink,
    );
    assert!(participant.upstream.owns_managed_track(video_id));
    assert!(participant.pending_keyframe_requests.is_empty());
    participant.enqueue_remote_keyframe((video_id, encodings[0]), KeyframeRequestKind::Pli);
    assert!(participant.pending_keyframe_requests.is_empty());
    participant.complete_native_resource_cutoff(7, true);
    assert!(participant.native_rtp.publications.is_empty());
    assert!(participant.pending_keyframe_requests.is_empty());
    assert!(participant.last_keyframe_request.is_empty());
    assert!(participant.disconnect_reason.is_none());
}
