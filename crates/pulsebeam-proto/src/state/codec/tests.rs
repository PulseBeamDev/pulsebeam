#![allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::unwrap_used
)]

use alloc::{string::String, vec};

use super::*;
use crate::state::*;
use validation::ValidationError;

fn publication(revision: u64) -> ClientStateBatch {
    ClientStateBatch {
        records: vec![ClientStateRecord {
            owned: revision,
            execution: None,
            desired: Some(DesiredState {
                revision,
                value: Some(desired_state::Value::Present(
                    ResourceSpec {
                        settings: Some(ResourceSettings {
                            enabled: Some(false),
                            receive: None,
                        }),
                        declaration: Some(resource_spec::Declaration::Publication(
                            PublicationSpec {
                                label: String::from("cursor"),
                                kind: Some(publication_spec::Kind::Data(DataPublication {
                                    delivery: DataDelivery::Lossy as i32,
                                    binding: Some(TrackBinding {
                                        transport: Some(track_binding::Transport::Sctp(
                                            SctpParameters { stream_id: Some(1) },
                                        )),
                                    }),
                                })),
                            },
                        )),
                    }
                    .into(),
                )),
            }),
        }],
    }
}

fn desired(batch: &mut ClientStateBatch) -> &mut DesiredState {
    batch.records[0].desired.as_mut().unwrap()
}

#[test]
fn raw_protobuf_preserves_exact_revision_and_explicit_false() {
    for revision in [1, (1 << 53) + 1, u64::MAX] {
        let mut batch = publication(revision);
        let protobuf = batch.encode_to_vec();
        assert_eq!(encode_client(&batch), Ok(protobuf.clone()));
        assert_eq!(decode_client(&protobuf), Ok(batch.clone()));
        let Some(desired_state::Value::Present(spec)) = &desired(&mut batch).value else {
            panic!("present")
        };
        assert_eq!(validation::validate_spec(spec), Ok(()));
        assert_eq!(spec.settings.as_ref().unwrap().enabled, Some(false));
    }
}

#[test]
fn server_marker_and_records_cannot_be_client_proposals() {
    let marker = ServerStateBatch {
        initial_complete: true,
        records: vec![],
    };
    assert_eq!(encode_server(&marker), Ok(vec![0x10, 1]));
    assert_eq!(decode_server(&[0x10, 1]), Ok(marker));
    assert_eq!(
        decode_client(&[0x10, 1]),
        Err(DecodeError::Validation(ValidationError::Batch))
    );
    assert_eq!(
        encode_server(&ServerStateBatch::default()),
        Err(EncodeError::Validation(ValidationError::Batch))
    );
    assert_eq!(
        encode_client(&ClientStateBatch::default()),
        Err(EncodeError::Validation(ValidationError::Batch))
    );
    let observation = ServerStateBatch {
        initial_complete: false,
        records: vec![ServerStateRecord {
            key: Some(ResourceKey {
                key: Some(resource_key::Key::Owned(1)),
            }),
            confirmed: Some(ConfirmedState {
                evaluated_revision: 1,
                value: Some(confirmed_state::Value::Absent(AbsentState {
                    reason: AbsenceReason::Removed as i32,
                })),
                ..Default::default()
            }),
        }],
    };
    assert!(decode_client(&encode_server(&observation).unwrap()).is_err());
    assert!(decode_server(&encode_client(&publication(1)).unwrap()).is_err());
}

#[test]
fn desired_envelopes_reject_illegal_refs_and_record_order_but_not_invalid_specs() {
    let mut batch = publication(1);
    batch.records[0].owned = 0;
    assert_eq!(
        encode_client(&batch),
        Err(EncodeError::Validation(ValidationError::Key))
    );
    batch.records[0].owned = 2;
    assert_eq!(
        encode_client(&batch),
        Err(EncodeError::Validation(ValidationError::Desired))
    );
    batch.records[0].owned = 1;
    desired(&mut batch).value = Some(desired_state::Value::Absent(true));
    assert_eq!(
        encode_client(&batch),
        Err(EncodeError::Validation(ValidationError::Desired))
    );
    desired(&mut batch).revision = 3;
    assert!(encode_client(&batch).is_ok());
    desired(&mut batch).value = Some(desired_state::Value::Absent(false));
    assert_eq!(
        encode_client(&batch),
        Err(EncodeError::Validation(ValidationError::Desired))
    );
    desired(&mut batch).value = Some(desired_state::Value::Present(
        ResourceSpec::default().into(),
    ));
    assert!(encode_client(&batch).is_ok());
    assert_eq!(
        validation::validate_spec(&ResourceSpec::default()),
        Err(ValidationError::Settings)
    );
    batch.records.push(publication(2).records.remove(0));
    assert_eq!(
        encode_client(&batch),
        Err(EncodeError::Validation(ValidationError::Desired))
    );
}

#[test]
fn raw_messages_reject_truncation_trailing_garbage_and_legacy_framing() {
    let protobuf = encode_client(&publication(1)).unwrap();
    for end in 0..protobuf.len() {
        assert!(decode_client(&protobuf[..end]).is_err());
    }
    let mut trailing = protobuf.clone();
    trailing.push(0);
    assert!(decode_client(&trailing).is_err());
    let mut legacy = vec![0];
    legacy.extend_from_slice(&u32::try_from(protobuf.len()).unwrap().to_le_bytes());
    legacy.extend_from_slice(&protobuf);
    assert!(decode_client(&legacy).is_err());
    assert_eq!(
        decode_client(&vec![0; MAX_FRAME_SIZE + 1]),
        Err(DecodeError::FrameSize)
    );
}

#[test]
fn exact_raw_ceiling_and_oversize_have_no_alternative_encoding() {
    let batch = publication(1);
    let mut protobuf = batch.encode_to_vec();
    // Unknown descriptive field 31 is ignored, but still counts toward admission.
    let padding = MAX_FRAME_SIZE - protobuf.len() - 4;
    protobuf.extend_from_slice(&[
        0xfa,
        1,
        u8::try_from((padding & 127) | 128).unwrap(),
        u8::try_from(padding >> 7).unwrap(),
    ]);
    protobuf.resize(MAX_FRAME_SIZE, b'x');
    assert_eq!(decode_client(&protobuf), Ok(batch));
    protobuf.push(b'x');
    assert_eq!(decode_client(&protobuf), Err(DecodeError::FrameSize));
}

#[test]
fn structural_preflight_bounds_repeated_empty_messages_groups_and_bad_varints() {
    let full = ClientStateBatch {
        records: (1..=u64::try_from(MAX_RECORDS).unwrap())
            .map(|r| publication(r).records.remove(0))
            .collect(),
    };
    assert_eq!(decode_client(&encode_client(&full).unwrap()), Ok(full));
    let records = [0x0a, 0].repeat(MAX_RECORDS + 1);
    assert_eq!(decode_client(&records), Err(DecodeError::Structure));
    assert_eq!(decode_server(&records), Err(DecodeError::Structure));
    let mut nested = vec![0x0b; 17];
    nested.extend_from_slice(&[0x0c; 17]);
    assert_eq!(decode_server(&nested), Err(DecodeError::Structure));
    for bytes in [
        &[0][..],
        &[0x80],
        &[
            0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 2,
        ],
        &[0x0b, 0x14],
    ] {
        assert_eq!(decode_server(bytes), Err(DecodeError::InvalidProtobuf));
    }
    // Unknown groups and duplicate singular messages retain protobuf merge semantics.
    let bytes = [0xfb, 1, 8, 1, 0xfc, 1, 0x10, 1];
    assert_eq!(
        decode_server(&bytes),
        Ok(ServerStateBatch {
            initial_complete: true,
            records: vec![]
        })
    );
    let batch = publication(1);
    let mut record = batch.records[0].encode_to_vec();
    record.extend_from_slice(&[0x12, 0]);
    let mut duplicate = vec![0x0a, u8::try_from(record.len()).unwrap()];
    duplicate.extend_from_slice(&record);
    assert_eq!(decode_client(&duplicate), Ok(batch.clone()));

    // Merge a future ResourceSpec field 31 = true into desired.present.
    // Unknown requirements are discarded, not negotiated or preserved.
    let mut record = batch.records[0].encode_to_vec();
    record.extend_from_slice(&[0x12, 5, 0x12, 3, 0xf8, 1, 1]);
    let mut future = vec![0x0a, u8::try_from(record.len()).unwrap()];
    future.extend_from_slice(&record);
    let decoded = decode_client(&future).unwrap();
    assert_eq!(decoded, batch);
    let canonical = encode_client(&decoded).unwrap();
    assert_eq!(canonical, encode_client(&batch).unwrap());
    assert_ne!(canonical, future);
}

fn rtp() -> RtpParameters {
    RtpParameters {
        mid: String::from("0"),
        codec: Some(RtpCodecParameters {
            payload_type: Some(0),
            rtx_payload_type: None,
            feedback: vec![RtcpFeedback::TransportCc.into()],
            format: Some(CodecFormat::from(codec_format::Codec::Opus(
                OpusParameters::default(),
            ))),
        }),
        header_extensions: vec![
            RtpHeaderExtensionParameters {
                uri: RtpHeaderExtensionType::RtpHeaderExtensionMid.into(),
                id: 1,
            },
            RtpHeaderExtensionParameters {
                uri: RtpHeaderExtensionType::RtpHeaderExtensionTransportCcDraft01.into(),
                id: 4,
            },
        ],
        encodings: vec![RtpEncodingParameters {
            ssrc: Some(0),
            rtx_ssrc: None,
            rid: String::new(),
            scalability_mode: None,
        }],
        rtcp: Some(RtcpParameters {
            cname: String::from("audio"),
            reduced_size: false,
        }),
    }
}

#[test]
fn rtp_presence_and_source_coordinates_are_not_conflated_with_zero() {
    let mut binding = rtp();
    let mut batch = publication(1);
    let Some(desired_state::Value::Present(spec)) = desired(&mut batch).value.as_mut() else {
        panic!("present")
    };
    spec.declaration = Some(resource_spec::Declaration::Publication(PublicationSpec {
        label: String::from("mic"),
        kind: Some(publication_spec::Kind::Audio(AudioPublication {
            binding: Some(TrackBinding {
                transport: Some(track_binding::Transport::Rtp(binding.clone())),
            }),
        })),
    }));
    assert_eq!(decode_client(&encode_client(&batch).unwrap()), Ok(batch));
    assert_eq!(
        validation::validate_rtp(&binding, TrackKind::Audio, true),
        Ok(())
    );
    binding.encodings[0].ssrc = None;
    assert_eq!(
        validation::validate_rtp(&binding, TrackKind::Audio, true),
        Err(ValidationError::Binding)
    );
    binding.encodings[0].ssrc = Some(0);
    binding.codec.as_mut().unwrap().payload_type = None;
    assert_eq!(
        validation::validate_rtp(&binding, TrackKind::Audio, true),
        Err(ValidationError::Binding)
    );
    for pt in [64, 95, 128] {
        binding.codec.as_mut().unwrap().payload_type = Some(pt);
        assert_eq!(
            validation::validate_rtp(&binding, TrackKind::Audio, true),
            Err(ValidationError::Binding)
        );
    }
    binding = rtp();
    binding.mid = String::from("invalid:mid");
    assert_eq!(
        validation::validate_rtp(&binding, TrackKind::Audio, true),
        Err(ValidationError::Binding)
    );
    binding = rtp();
    binding.codec.as_mut().unwrap().rtx_payload_type = Some(1);
    assert_eq!(
        validation::validate_rtp(&binding, TrackKind::Audio, true),
        Err(ValidationError::Binding)
    );
    binding.encodings[0].rtx_ssrc = Some(0);
    assert_eq!(
        validation::validate_rtp(&binding, TrackKind::Audio, true),
        Err(ValidationError::Binding)
    );
    binding.encodings[0].rtx_ssrc = Some(1);
    assert_eq!(
        validation::validate_rtp(&binding, TrackKind::Audio, true),
        Ok(())
    );
    binding
        .header_extensions
        .push(binding.header_extensions[0].clone());
    assert_eq!(
        validation::validate_rtp(&binding, TrackKind::Audio, true),
        Err(ValidationError::Binding)
    );
}

#[test]
fn rtp_validity_does_not_select_congestion_control() {
    let mut binding = rtp();
    binding.codec.as_mut().unwrap().feedback.clear();
    binding.header_extensions.retain(|extension| {
        extension.uri != RtpHeaderExtensionType::RtpHeaderExtensionTransportCcDraft01.as_str()
    });
    assert_eq!(
        validation::validate_rtp(&binding, TrackKind::Audio, true),
        Ok(())
    );

    binding
        .codec
        .as_mut()
        .unwrap()
        .feedback
        .push("future-cc".into());
    binding
        .header_extensions
        .push(RtpHeaderExtensionParameters {
            id: 4,
            uri: "urn:example:future-cc".into(),
        });
    for publication in [false, true] {
        assert_eq!(
            validation::validate_rtp(&binding, TrackKind::Audio, publication),
            Ok(())
        );
    }
}

#[test]
fn receive_replacements_preserve_explicit_zero_and_reject_not_clamp() {
    let spec = |options| ResourceSpec {
        settings: Some(ResourceSettings {
            enabled: Some(true),
            receive: Some(resource_settings::Receive::Video(options)),
        }),
        declaration: Some(resource_spec::Declaration::Subscription(SubscriptionSpec {
            track_id: String::from("opaque-track"),
            kind: TrackKind::Video as i32,
            binding: Some(TrackBinding {
                transport: Some(track_binding::Transport::Rtp(RtpParameters {
                    codec: Some(RtpCodecParameters {
                        format: Some(CodecFormat::from(codec_format::Codec::Vp8(
                            Vp8Parameters {},
                        ))),
                        ..rtp().codec.unwrap()
                    }),
                    ..rtp()
                })),
            }),
        })),
    };
    let defaults = VideoReceiveOptions::default();
    assert_eq!(validation::validate_spec(&spec(defaults.clone())), Ok(()));
    let minimum_delay = VideoReceiveOptions {
        playout_delay: Some(PlayoutDelay {
            min_ms: 0,
            max_ms: 0,
        }),
        ..defaults.clone()
    };
    assert_eq!(validation::validate_spec(&spec(minimum_delay)), Ok(()));
    for invalid in [
        VideoReceiveOptions {
            max_height: Some(0),
            ..defaults.clone()
        },
        VideoReceiveOptions {
            min_height: Some(20),
            max_height: Some(10),
            ..defaults.clone()
        },
        VideoReceiveOptions {
            min_fps: Some(241),
            ..defaults.clone()
        },
        VideoReceiveOptions {
            priority: Some(0),
            ..defaults.clone()
        },
        VideoReceiveOptions {
            playout_delay: Some(PlayoutDelay {
                min_ms: 1,
                max_ms: 0,
            }),
            ..defaults.clone()
        },
        VideoReceiveOptions {
            playout_delay: Some(PlayoutDelay {
                min_ms: 0,
                max_ms: 4096,
            }),
            ..defaults.clone()
        },
    ] {
        assert_eq!(
            validation::validate_spec(&spec(invalid)),
            Err(ValidationError::Settings)
        );
    }
}

#[test]
fn owned_confirmation_is_self_contained_and_execution_failure_is_not_rejection() {
    let Some(desired_state::Value::Present(spec)) = desired(&mut publication(1)).value.take()
    else {
        panic!("present")
    };
    let live = LiveResource {
        spec: Some(*spec),
        participant: Some(Participant {
            participant_id: String::from("participant"),
            participant_external_id: String::from("alice"),
        }),
        track: Some(Track {
            track_id: String::from("track"),
            participant_id: String::from("participant"),
            label: String::from("cursor"),
            kind: Some(track::Kind::Data(DataTrack {
                delivery: DataDelivery::Lossy as i32,
            })),
        }),
        execution: ExecutionStatus::Disabled as i32,
        execution_failure: None,
        activity: None,
    };
    let mut confirmed = ConfirmedState {
        evaluated_revision: 1,
        rejection: Some(Failure {
            code: FailureCode::InvalidConfiguration as i32,
            diagnostic: String::from("Rejected update; accepted state retained"),
        }),
        value: Some(confirmed_state::Value::Owned(live.clone().into())),
    };
    let batch = |confirmed| ServerStateBatch {
        initial_complete: false,
        records: vec![ServerStateRecord {
            key: Some(ResourceKey {
                key: Some(resource_key::Key::Owned(1)),
            }),
            confirmed: Some(confirmed),
        }],
    };
    let accepted = batch(confirmed.clone());
    assert_eq!(
        decode_server(&encode_server(&accepted).unwrap()),
        Ok(accepted)
    );
    let Some(confirmed_state::Value::Owned(current)) = confirmed.value.as_mut() else {
        panic!("owned")
    };
    current.execution = ExecutionStatus::Failed as i32;
    assert_eq!(
        validation::validate_live(current),
        Err(ValidationError::Failure)
    );
    current.execution_failure = Some(Failure {
        code: FailureCode::InternalFailure as i32,
        diagnostic: String::new(),
    });
    assert_eq!(validation::validate_live(current), Ok(()));
    current.track.as_mut().unwrap().participant_id = String::from("different-source");
    assert_eq!(
        validation::validate_live(current),
        Err(ValidationError::Identity)
    );
    let mut invalid = live;
    invalid.execution = ExecutionStatus::Installed as i32;
    assert_eq!(
        validation::validate_live(&invalid),
        Err(ValidationError::Confirmed)
    );
    invalid.execution = ExecutionStatus::WaitingForSource as i32;
    invalid
        .spec
        .as_mut()
        .unwrap()
        .settings
        .as_mut()
        .unwrap()
        .enabled = Some(true);
    assert_eq!(
        validation::validate_live(&invalid),
        Err(ValidationError::Confirmed)
    );
}

#[test]
fn immutable_declarations_use_codec_defaults_without_reassociating_sources() {
    let video = |binding: RtpParameters| ResourceSpec {
        settings: Some(ResourceSettings {
            enabled: Some(true),
            receive: None,
        }),
        declaration: Some(resource_spec::Declaration::Publication(PublicationSpec {
            label: String::from("camera"),
            kind: Some(publication_spec::Kind::Video(VideoPublication {
                binding: Some(TrackBinding {
                    transport: Some(track_binding::Transport::Rtp(binding)),
                }),
            })),
        })),
    };
    let mut binding = rtp();
    binding
        .header_extensions
        .push(RtpHeaderExtensionParameters {
            uri: RtpHeaderExtensionType::RtpHeaderExtensionRid.into(),
            id: 2,
        });
    binding.codec.as_mut().unwrap().format =
        Some(CodecFormat::from(codec_format::Codec::Vp9(Vp9Parameters {
            profile_id: None,
        })));
    binding
        .codec
        .as_mut()
        .unwrap()
        .feedback
        .push(RtcpFeedback::Nack.into());
    let original = video(binding.clone());
    let mut explicit = binding.clone();
    explicit.codec.as_mut().unwrap().format =
        Some(CodecFormat::from(codec_format::Codec::Vp9(Vp9Parameters {
            profile_id: Some(0),
        })));
    explicit.codec.as_mut().unwrap().feedback.reverse();
    explicit.header_extensions.reverse();
    let mut equivalent = video(explicit);
    equivalent.settings.as_mut().unwrap().enabled = Some(false);
    assert_eq!(validation::validate_spec(&original), Ok(()));
    assert_eq!(validation::validate_spec(&equivalent), Ok(()));
    assert!(validation::declarations_equivalent(&original, &equivalent));
    assert!(validation::declarations_equivalent(&equivalent, &original));
    assert!(!validation::settings_equivalent(
        original.settings.as_ref().unwrap(),
        equivalent.settings.as_ref().unwrap()
    ));
    let changes: [fn(&mut RtpParameters); 8] = [
        |v| v.mid = String::from("replacement"),
        |v| v.encodings[0].ssrc = Some(1),
        |v| {
            v.codec.as_mut().unwrap().rtx_payload_type = Some(97);
            v.encodings[0].rtx_ssrc = Some(2);
        },
        |v| v.encodings[0].rid = String::from("other"),
        |v| v.encodings[0].scalability_mode = Some(RtpScalabilityMode::L1t1.as_str().into()),
        |v| v.rtcp.as_mut().unwrap().cname = String::from("other"),
        |v| v.rtcp.as_mut().unwrap().reduced_size = true,
        |v| v.header_extensions[0].id = 10,
    ];
    for change in changes {
        let mut changed = binding.clone();
        change(&mut changed);
        let changed = video(changed);
        assert_eq!(validation::validate_spec(&changed), Ok(()));
        assert!(!validation::declarations_equivalent(&original, &changed));
    }
    let subscription = |track_id: &str| ResourceSpec {
        settings: Some(ResourceSettings {
            enabled: Some(true),
            receive: Some(resource_settings::Receive::Video(
                VideoReceiveOptions::default(),
            )),
        }),
        declaration: Some(resource_spec::Declaration::Subscription(SubscriptionSpec {
            track_id: String::from(track_id),
            kind: TrackKind::Video as i32,
            binding: Some(TrackBinding {
                transport: Some(track_binding::Transport::Rtp(binding.clone())),
            }),
        })),
    };
    assert!(!validation::declarations_equivalent(
        &subscription("source-a"),
        &subscription("source-b")
    ));
    assert!(!validation::declarations_equivalent(
        &original,
        &subscription("source-a")
    ));

    let mut data = publication(1);
    let Some(desired_state::Value::Present(data)) = desired(&mut data).value.as_ref() else {
        panic!("present")
    };
    let mut moved = data.clone();
    let Some(resource_spec::Declaration::Publication(p)) = &mut moved.declaration else {
        panic!("publication")
    };
    let Some(publication_spec::Kind::Data(d)) = &mut p.kind else {
        panic!("data")
    };
    let Some(track_binding::Transport::Sctp(sctp)) = d.binding.as_mut().unwrap().transport.as_mut()
    else {
        panic!("sctp")
    };
    sctp.stream_id = Some(2);
    assert!(!validation::declarations_equivalent(data, &moved));
}

#[test]
fn mutable_defaults_do_not_erase_presence_or_explicit_playout_requests() {
    let settings = |options| ResourceSettings {
        enabled: Some(false),
        receive: Some(resource_settings::Receive::Video(options)),
    };
    let default = settings(VideoReceiveOptions::default());
    let mut options = VideoReceiveOptions {
        priority: Some(1),
        ..VideoReceiveOptions::default()
    };
    assert!(validation::settings_equivalent(
        &default,
        &settings(options.clone())
    ));
    options.priority = Some(2);
    assert!(!validation::settings_equivalent(
        &default,
        &settings(options.clone())
    ));
    options.priority = None;
    options.playout_delay = Some(PlayoutDelay {
        min_ms: 0,
        max_ms: 0,
    });
    assert!(!validation::settings_equivalent(
        &default,
        &settings(options)
    ));
    let opus = |dtx| {
        CodecFormat::from(codec_format::Codec::Opus(OpusParameters {
            dtx,
            ..OpusParameters::default()
        }))
    };
    assert!(!validation::formats_equivalent(
        &opus(None),
        &opus(Some(false))
    ));
    let format = |parameters| CodecFormat::from(codec_format::Codec::Opus(parameters));
    for parameters in [
        OpusParameters {
            max_playback_rate_hz: Some(48000),
            ..OpusParameters::default()
        },
        OpusParameters {
            max_capture_rate_hz: Some(48000),
            ..OpusParameters::default()
        },
    ] {
        let explicit = format(parameters);
        assert_ne!(opus(None), explicit);
        assert!(validation::formats_equivalent(&opus(None), &explicit));
        assert!(validation::formats_equivalent(&explicit, &opus(None)));
    }
    let changes: [fn(&mut OpusParameters); 6] = [
        |v| v.stereo = Some(false),
        |v| v.source_stereo = Some(false),
        |v| v.in_band_fec = Some(false),
        |v| v.constant_bitrate = Some(false),
        |v| v.max_average_bitrate_bps = Some(510000),
        |v| v.min_packet_time_ms = Some(10),
    ];
    for change in changes {
        let mut parameters = OpusParameters::default();
        change(&mut parameters);
        assert!(!validation::formats_equivalent(
            &opus(None),
            &format(parameters)
        ));
    }
    assert!(!validation::formats_equivalent(
        &opus(None),
        &format(OpusParameters {
            max_playback_rate_hz: Some(16000),
            ..OpusParameters::default()
        })
    ));
    let h264 = |asymmetry| {
        CodecFormat::from(codec_format::Codec::H264(H264Parameters {
            profile_level_id: Some(0x42e01f),
            level_asymmetry_allowed: asymmetry,
        }))
    };
    assert!(validation::formats_equivalent(
        &h264(None),
        &h264(Some(false))
    ));
    assert!(!validation::formats_equivalent(
        &h264(None),
        &h264(Some(true))
    ));
}

#[test]
fn capability_identity_uses_typed_defaults_without_guessing_unsupported_formats() {
    let default = CodecFormat::from(codec_format::Codec::Vp9(Vp9Parameters { profile_id: None }));
    let explicit = CodecFormat::from(codec_format::Codec::Vp9(Vp9Parameters {
        profile_id: Some(0),
    }));
    assert!(validation::formats_equivalent(&default, &explicit));
    let capability = |format| RtpCodecCapability {
        format: Some(format),
        feedback: vec![RtcpFeedback::TransportCc.into()],
        rtx_supported: false,
        scalability_modes: vec![],
    };
    let mut caps = RtpCapabilities {
        codecs: vec![capability(default)],
        header_extensions: vec![
            RtpHeaderExtensionType::RtpHeaderExtensionMid.into(),
            RtpHeaderExtensionType::RtpHeaderExtensionTransportCcDraft01.into(),
            RtpHeaderExtensionType::RtpHeaderExtensionVideoLayersAllocation.into(),
        ],
        rtcp_modes: vec![RtcpMode::Compound.into()],
    };
    assert_eq!(validation::validate_capabilities(&caps), Ok(()));
    #[cfg(feature = "json")]
    {
        use serde_json::{from_str, from_value, json, to_value};
        let encoded = to_value(&caps).unwrap();
        assert_eq!(
            encoded["codecs"][0]["format"],
            json!({"mime_type":"video/vp9","clock_rate":90000})
        );
        assert_eq!(encoded["codecs"][0]["feedback"], json!(["transport-cc"]));
        assert_eq!(encoded["rtcp_modes"], json!(["compound"]));
        assert_eq!(
            from_value::<RtpCapabilities>(encoded.clone()).unwrap(),
            caps
        );
        let explicit_json = to_value(&explicit).unwrap();
        assert_eq!(
            explicit_json["parameters"],
            json!([{"name":"profile-id","value":"0"}])
        );
        assert_eq!(from_value::<CodecFormat>(explicit_json).unwrap(), explicit);
        let opus = CodecFormat::from(codec_format::Codec::Opus(OpusParameters {
            stereo: Some(false),
            source_stereo: Some(false),
            in_band_fec: Some(false),
            dtx: Some(false),
            constant_bitrate: Some(false),
            ..Default::default()
        }));
        let opus_json = to_value(&opus).unwrap();
        assert_eq!(opus_json["parameters"].as_array().unwrap().len(), 5);
        assert!(
            opus_json["parameters"]
                .as_array()
                .unwrap()
                .iter()
                .all(|parameter| parameter["value"] == "0")
        );
        assert_eq!(from_value::<CodecFormat>(opus_json).unwrap(), opus);
        assert_eq!(to_value(RtpCapabilities::default()).unwrap(), json!({}));
        let deliveries = [
            DataDelivery::Latest,
            DataDelivery::Lossy,
            DataDelivery::Ordered,
        ];
        assert_eq!(
            to_value(deliveries).unwrap(),
            json!(["LATEST", "LOSSY", "ORDERED"])
        );
        assert_eq!(
            from_value::<[DataDelivery; 3]>(json!(["LATEST", "LOSSY", "ORDERED"])).unwrap(),
            deliveries
        );
        assert!(
            from_str::<RtpCapabilities>(
                r#"{"rtcp_modes":["compound"],"rtcp_modes":["reduced-size"]}"#
            )
            .is_err()
        );
        let mut future = encoded.clone();
        future["future_optional"] = json!({"enabled":true});
        assert_eq!(from_value::<RtpCapabilities>(future).unwrap(), caps);
        assert!(from_str::<RtpCapabilities>("{} {}").is_err());
        let mut invalid = encoded;
        invalid["header_extensions"][0] = json!("");
        assert_eq!(
            validation::validate_capabilities(&from_value::<RtpCapabilities>(invalid).unwrap()),
            Err(ValidationError::Capability)
        );
    }
    caps.codecs.push(capability(explicit));
    assert_eq!(
        validation::validate_capabilities(&caps),
        Err(ValidationError::Capability)
    );
    assert_eq!(
        validation::validate_format(&CodecFormat::default()),
        Err(ValidationError::Format)
    );
    let future_format = CodecFormat {
        mime_type: "video/future-codec".into(),
        clock_rate: u32::MAX,
        channels: None,
        parameters: vec![CodecParameter {
            name: "future-hint".into(),
            value: "1".into(),
        }],
    };
    assert_eq!(future_format.known_codec(), None);
    let unknown = Track {
        track_id: String::from("track"),
        participant_id: String::from("participant"),
        label: String::from("camera"),
        kind: Some(track::Kind::Video(VideoTrack {
            format: Some(future_format.clone()),
        })),
    };
    assert_eq!(validation::validate_track(&unknown), Ok(()));
    caps.codecs.pop();
    caps.codecs.push(capability(future_format));
    caps.header_extensions
        .push("urn:future:optional-extension".into());
    caps.codecs[0].feedback.push("future-feedback".into());
    assert_eq!(validation::validate_capabilities(&caps), Ok(()));
    let mut missing = unknown;
    missing.kind = Some(track::Kind::Video(VideoTrack { format: None }));
    assert_eq!(
        validation::validate_track(&missing),
        Err(ValidationError::Format)
    );
}

#[test]
fn session_and_owned_absence_have_distinct_failure_and_direction_rules() {
    let confirmed = |key, state| ServerStateBatch {
        initial_complete: false,
        records: vec![ServerStateRecord {
            key: Some(ResourceKey { key: Some(key) }),
            confirmed: Some(state),
        }],
    };
    let grant = SessionState {
        authorization: Some(Authorization {
            expires_at_unix_seconds: i64::MAX,
            revision: u64::MAX,
        }),
        closed: None,
        diagnostic: String::new(),
    };
    let mut session = ConfirmedState {
        value: Some(confirmed_state::Value::Session(grant.clone())),
        ..ConfirmedState::default()
    };
    assert!(
        encode_server(&confirmed(
            resource_key::Key::Session(true),
            session.clone()
        ))
        .is_ok()
    );
    session.evaluated_revision = 1;
    assert_eq!(
        encode_server(&confirmed(resource_key::Key::Session(true), session)),
        Err(EncodeError::Validation(ValidationError::Confirmed))
    );
    let mut absent = ConfirmedState {
        value: Some(confirmed_state::Value::Absent(AbsentState {
            reason: AbsenceReason::CreateRejected as i32,
        })),
        ..ConfirmedState::default()
    };
    assert!(encode_server(&confirmed(resource_key::Key::Owned(1), absent.clone())).is_err());
    absent.rejection = Some(Failure {
        code: FailureCode::InvalidConfiguration as i32,
        diagnostic: String::new(),
    });
    let batch = confirmed(resource_key::Key::Owned(1), absent);
    assert_eq!(decode_server(&encode_server(&batch).unwrap()), Ok(batch));
    let terminal = ConfirmedState {
        value: Some(confirmed_state::Value::Session(SessionState {
            closed: Some(CloseReason::AuthorizationRevoked as i32),
            diagnostic: String::from("Grant revoked"),
            ..grant
        })),
        ..ConfirmedState::default()
    };
    let batch = confirmed(resource_key::Key::Session(true), terminal);
    assert_eq!(decode_server(&encode_server(&batch).unwrap()), Ok(batch));
}

#[test]
fn execution_failures_are_required_scoped_and_independently_revisioned() {
    let mut batch = ClientStateBatch {
        records: vec![ClientStateRecord {
            owned: 1,
            desired: None,
            execution: Some(ExecutionReport {
                revision: 2,
                failure: Some(Failure {
                    code: FailureCode::InternalFailure as i32,
                    diagnostic: String::from("receiver failed after admission"),
                }),
            }),
        }],
    };
    assert_eq!(
        decode_client(&encode_client(&batch).unwrap()),
        Ok(batch.clone())
    );
    batch.records[0].execution.as_mut().unwrap().failure = None;
    assert_eq!(
        encode_client(&batch),
        Err(EncodeError::Validation(ValidationError::Failure))
    );
    batch.records[0].execution.as_mut().unwrap().failure = Some(Failure {
        code: FailureCode::InternalFailure as i32,
        diagnostic: String::new(),
    });
    batch.records[0].execution.as_mut().unwrap().revision = 1;
    assert_eq!(
        encode_client(&batch),
        Err(EncodeError::Validation(ValidationError::Desired))
    );
    batch.records[0].execution.as_mut().unwrap().revision = 2;
    batch.records[0].desired = publication(3).records.pop().unwrap().desired;
    assert_eq!(
        encode_client(&batch),
        Err(EncodeError::Validation(ValidationError::Desired))
    );
}
