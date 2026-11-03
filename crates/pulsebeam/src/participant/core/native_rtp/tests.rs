use super::*;
mod receiver;
use crate::participant::core::native_data::tests::{
    confirmations, frame, participant_with_capabilities,
};
use crate::participant::event::test_utils::MockParticipantSink;

fn opus() -> wire::CodecFormat {
    wire::CodecFormat {
        codec: Some(wire::codec_format::Codec::Opus(
            wire::OpusParameters::default(),
        )),
    }
}

fn capabilities() -> wire::RtpCapabilities {
    let video = crate::participant::rtp::capabilities()
        .codecs
        .into_iter()
        .find(|codec| {
            matches!(
                codec
                    .format
                    .as_ref()
                    .and_then(|format| format.codec.as_ref()),
                Some(wire::codec_format::Codec::H264(_))
            )
        })
        .unwrap();
    wire::RtpCapabilities {
        codecs: vec![
            wire::RtpCodecCapability {
                format: Some(opus()),
                feedback: vec![wire::RtcpFeedback::TransportCc as i32],
                rtx_supported: false,
                scalability_modes: vec![],
            },
            video,
        ],
        header_extensions: vec![
            wire::RtpHeaderExtensionType::RtpHeaderExtensionMid as i32,
            wire::RtpHeaderExtensionType::RtpHeaderExtensionTransportCcDraft01 as i32,
            wire::RtpHeaderExtensionType::RtpHeaderExtensionPlayoutDelay as i32,
            wire::RtpHeaderExtensionType::RtpHeaderExtensionDependencyDescriptor as i32,
        ],
        rtcp_modes: vec![wire::RtcpMode::Compound as i32],
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
                    codec: Some(wire::RtpCodecParameters { payload_type: Some(111), rtx_payload_type: None, feedback: vec![wire::RtcpFeedback::TransportCc as i32], format: Some(opus()) }),
                    header_extensions: vec![wire::RtpHeaderExtensionParameters { id: 1, r#type: wire::RtpHeaderExtensionType::RtpHeaderExtensionMid as i32 }, wire::RtpHeaderExtensionParameters { id: 2, r#type: wire::RtpHeaderExtensionType::RtpHeaderExtensionTransportCcDraft01 as i32 }],
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
            codec
                .format
                .filter(|format| matches!(format.codec, Some(wire::codec_format::Codec::H264(_))))
        })
        .unwrap();
    let mut header_extensions = vec![
        wire::RtpHeaderExtensionParameters {
            id: 1,
            r#type: wire::RtpHeaderExtensionType::RtpHeaderExtensionMid as i32,
        },
        wire::RtpHeaderExtensionParameters {
            id: 2,
            r#type: wire::RtpHeaderExtensionType::RtpHeaderExtensionTransportCcDraft01 as i32,
        },
    ];
    if descriptor {
        header_extensions.push(wire::RtpHeaderExtensionParameters {
            id: 3,
            r#type: wire::RtpHeaderExtensionType::RtpHeaderExtensionDependencyDescriptor as i32,
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
                                        feedback: vec![wire::RtcpFeedback::TransportCc as i32],
                                        format: Some(format),
                                        ..Default::default()
                                    }),
                                    header_extensions,
                                    encodings: vec![wire::RtpEncodingParameters {
                                        ssrc: Some(ssrc),
                                        scalability_mode: Some(mode as i32),
                                        ..Default::default()
                                    }],
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
fn last_live(participant: &mut Participant) -> (wire::ConfirmedState, wire::LiveResource) {
    last_live_for(participant, 1)
}

#[track_caller]
fn last_live_for(
    participant: &mut Participant,
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
    assert!(!source.binding.encodings[0].active);
    receiver::check(&source.track);
    assert_eq!(sink.publish_track_calls, vec![id]);
    assert_eq!(participant.upstream.mid_for_track_id(id), None);
    assert!(participant.native_rtp.owns_encoding(0));

    participant.native_rtp_demand(id, true);
    let (_, live) = last_live(&mut participant);
    assert_eq!(live.activity, Some(wire::MediaActivity::Active as i32));
    assert!(participant.native_rtp.publications[&1].binding.encodings[0].active);
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
    assert!(!participant.native_rtp.publications[&1].binding.encodings[0].active);
    assert_eq!(participant.native_rtp.publications[&1].track.id(), id);
    participant.handle_native_signaling(
        &frame(3, wire::desired_state::Value::Present(Box::new(spec(true)))),
        &mut sink,
    );
    let (_, live) = last_live(&mut participant);
    assert_eq!(live.activity, Some(wire::MediaActivity::Active as i32));
    assert!(participant.native_rtp.publications[&1].binding.encodings[0].active);
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
    assert!(!participant.native_rtp.publications[&1].binding.encodings[0].active);

    participant.handle_native_signaling(
        &frame(5, wire::desired_state::Value::Absent(true)),
        &mut sink,
    );
    assert!(participant.native_rtp.publications[&1].retiring);
    assert!(
        participant.native_rtp.tracks.is_empty()
            && participant.native_rtp.mids.is_empty()
            && participant.native_rtp.ssrcs.is_empty()
            && participant.native_rtp.fanouts.is_empty()
            && participant.native_rtp.active.is_empty()
    );
    assert_eq!(sink.unpublish_track_calls, vec![id]);
    participant.retire_native_resource(1, &mut sink);
    assert_eq!(sink.unpublish_track_calls, vec![id]);
    assert!(participant.native_rtp.publications.contains_key(&1));
    participant.native_rtp_demand(id, true);
    assert!(participant.native_rtp.publications[&1].retiring);
    participant.complete_native_resource_cutoff(1, true);
    let absent = confirmations(&mut participant).pop().unwrap();
    assert!(matches!(
        absent.value,
        Some(wire::confirmed_state::Value::Absent(_))
    ));
    assert!(participant.native_rtp.publications.is_empty());
    assert!(participant.disconnect_reason.is_none());

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
    let video_binding = participant.native_rtp.publications[&7].binding.clone();
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
        participant.native_rtp.publications[&7].binding,
        video_binding
    );
    assert_eq!(sink.publish_track_calls, vec![id, video_id]);
    participant.handle_native_signaling(
        &native_data::tests::frame_for(7, 9, wire::desired_state::Value::Absent(true)),
        &mut sink,
    );
    participant.complete_native_resource_cutoff(7, true);
    assert!(participant.native_rtp.publications.is_empty());
    assert!(participant.disconnect_reason.is_none());
}
