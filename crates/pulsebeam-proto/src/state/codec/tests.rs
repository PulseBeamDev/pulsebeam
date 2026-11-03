#![allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::unwrap_used
)]

use alloc::{string::String, vec, vec::Vec};
use lz4_flex::block::compress;

use super::*;
use crate::state::*;
use validation::{Direction::*, ValidationError};

fn publication(revision: u64) -> StateBatch {
    StateBatch {
        records: vec![StateRecord {
            key: Some(ResourceKey {
                key: Some(resource_key::Key::Owned(revision)),
            }),
            state: Some(state_record::State::Desired(DesiredState {
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
            })),
        }],
        initial_complete: false,
    }
}

fn raw(bytes: &[u8], encoding: u8, length: usize) -> Vec<u8> {
    let mut frame = vec![encoding];
    frame.extend_from_slice(&u32::try_from(length).unwrap().to_le_bytes());
    frame.extend_from_slice(bytes);
    frame
}

fn desired(batch: &mut StateBatch) -> &mut DesiredState {
    let Some(state_record::State::Desired(desired)) = &mut batch.records[0].state else {
        panic!("desired fixture")
    };
    desired
}

#[test]
fn both_encodings_preserve_exact_revision_and_explicit_false() {
    for revision in [1, (1 << 53) + 1, u64::MAX] {
        let mut batch = publication(revision);
        let protobuf = batch.encode_to_vec();
        for (encoding, payload) in [(0, protobuf.clone()), (1, compress(&protobuf))] {
            assert_eq!(
                decode(&raw(&payload, encoding, protobuf.len()), Client),
                Ok(batch.clone())
            );
        }
        assert_eq!(
            decode(&encode(&batch, Client).unwrap(), Client),
            Ok(batch.clone())
        );
        let Some(desired_state::Value::Present(spec)) = &desired(&mut batch).value else {
            panic!("present")
        };
        assert_eq!(validation::validate_spec(spec), Ok(()));
        assert_eq!(spec.settings.as_ref().unwrap().enabled, Some(false));
    }
}

#[test]
fn server_marker_is_not_a_client_message_or_an_empty_ordinary_batch() {
    let marker = StateBatch {
        initial_complete: true,
        records: vec![],
    };
    assert_eq!(encode(&marker, Server), Ok(vec![0, 2, 0, 0, 0, 0x10, 1]));
    assert_eq!(
        decode(&encode(&marker, Server).unwrap(), Server),
        Ok(marker.clone())
    );
    assert_eq!(
        encode(&marker, Client),
        Err(EncodeError::Validation(ValidationError::Batch))
    );
    assert_eq!(
        encode(&StateBatch::default(), Server),
        Err(EncodeError::Validation(ValidationError::Batch))
    );
    assert_eq!(
        encode(&publication(1), Server),
        Err(EncodeError::Validation(ValidationError::Direction))
    );
}

#[test]
fn desired_envelopes_reject_illegal_refs_and_record_order_but_not_invalid_specs() {
    let mut batch = publication(1);
    batch.records[0].key.as_mut().unwrap().key = Some(resource_key::Key::Owned(0));
    assert_eq!(
        encode(&batch, Client),
        Err(EncodeError::Validation(ValidationError::Key))
    );
    batch.records[0].key.as_mut().unwrap().key = Some(resource_key::Key::Owned(2));
    assert_eq!(
        encode(&batch, Client),
        Err(EncodeError::Validation(ValidationError::Desired))
    );
    batch.records[0].key.as_mut().unwrap().key = Some(resource_key::Key::Owned(1));
    desired(&mut batch).value = Some(desired_state::Value::Absent(true));
    assert_eq!(
        encode(&batch, Client),
        Err(EncodeError::Validation(ValidationError::Desired))
    );
    desired(&mut batch).revision = 3;
    assert!(encode(&batch, Client).is_ok());
    desired(&mut batch).value = Some(desired_state::Value::Absent(false));
    assert_eq!(
        encode(&batch, Client),
        Err(EncodeError::Validation(ValidationError::Desired))
    );
    desired(&mut batch).value = Some(desired_state::Value::Present(
        ResourceSpec::default().into(),
    ));
    assert!(encode(&batch, Client).is_ok());
    assert_eq!(
        validation::validate_spec(&ResourceSpec::default()),
        Err(ValidationError::Settings)
    );
    batch.records.push(publication(2).records.remove(0));
    assert_eq!(
        encode(&batch, Client),
        Err(EncodeError::Validation(ValidationError::Desired))
    );
}

#[test]
fn frame_and_lz4_lengths_are_exact_and_all_input_is_consumed() {
    let batch = publication(1);
    let protobuf = batch.encode_to_vec();
    for encoding in [0, 1] {
        let payload = if encoding == 0 {
            protobuf.clone()
        } else {
            compress(&protobuf)
        };
        for length in [
            0,
            protobuf.len() - 1,
            protobuf.len() + 1,
            MAX_DECODED_SIZE + 1,
        ] {
            assert!(decode(&raw(&payload, encoding, length), Client).is_err());
        }
        let mut trailing = raw(&payload, encoding, protobuf.len());
        trailing.push(0);
        assert!(decode(&trailing, Client).is_err());
        for end in 0..payload.len() {
            assert!(decode(&raw(&payload[..end], encoding, protobuf.len()), Client).is_err());
        }
    }
    assert_eq!(
        decode(&raw(&protobuf, 2, protobuf.len()), Client),
        Err(DecodeError::Encoding)
    );
    assert_eq!(
        decode(&vec![0; MAX_FRAME_SIZE + 1], Client),
        Err(DecodeError::FrameSize)
    );
    // Valid overlapping copy followed by terminal literals.
    assert_eq!(
        decode_lz4(
            &[0x1f, b'a', 1, 0, 2, 0x50, b'b', b'b', b'b', b'b', b'b'],
            27
        ),
        Ok([vec![b'a'; 22], vec![b'b'; 5]].concat())
    );
    // A safe decoder must still reject blocks violating LZ4's terminal
    // literal and last-match boundaries, even if they expand to exact length.
    assert_eq!(
        decode_lz4(&[0x10, b'a', 1, 0, 0], 5),
        Err(DecodeError::InvalidBlock)
    );
    assert_eq!(
        decode_lz4(&[0x10, b'a', 1, 0, 0x50, b'b', b'b', b'b', b'b', b'b'], 10),
        Err(DecodeError::InvalidBlock)
    );
    for block in [&[0, 0, 0][..], &[0, 1, 0], &[0xf0], &[0x10, b'a', 1, 0]] {
        assert!(decode_lz4(block, 32).is_err());
    }
}

#[test]
fn exact_raw_ceiling_and_compressible_oversize_do_not_bypass_admission() {
    let batch = publication(1);
    let mut protobuf = batch.encode_to_vec();
    // Unknown descriptive field 31 with bytes payload. Its wire representation
    // is valid and ignored; raw representability still bounds the whole batch.
    let padding = MAX_DECODED_SIZE - protobuf.len() - 4;
    protobuf.extend_from_slice(&[
        0xfa,
        1,
        u8::try_from((padding & 127) | 128).unwrap(),
        u8::try_from(padding >> 7).unwrap(),
    ]);
    protobuf.resize(MAX_DECODED_SIZE, b'x');
    let frame = raw(&protobuf, 0, protobuf.len());
    assert_eq!(frame.len(), MAX_FRAME_SIZE);
    assert_eq!(decode(&frame, Client), Ok(batch));
    protobuf.push(b'x');
    assert_eq!(
        decode(&raw(&compress(&protobuf), 1, protobuf.len()), Client),
        Err(DecodeError::DecodedSize)
    );
}

#[test]
fn structural_preflight_bounds_repeated_empty_messages_groups_and_bad_varints() {
    let full = StateBatch {
        initial_complete: false,
        records: (1..=u64::try_from(MAX_RECORDS).unwrap())
            .map(|r| publication(r).records.remove(0))
            .collect(),
    };
    assert_eq!(decode(&encode(&full, Client).unwrap(), Client), Ok(full));
    let records = [0x0a, 0].repeat(MAX_RECORDS + 1);
    assert_eq!(
        decode(&raw(&records, 0, records.len()), Client),
        Err(DecodeError::Structure)
    );
    let mut nested = vec![0x0b; 17];
    nested.extend_from_slice(&[0x0c; 17]);
    assert_eq!(
        decode(&raw(&nested, 0, nested.len()), Server),
        Err(DecodeError::Structure)
    );
    for bytes in [
        &[0][..],
        &[0x80],
        &[
            0x08, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 2,
        ],
        &[0x0b, 0x14],
    ] {
        assert_eq!(
            decode(&raw(bytes, 0, bytes.len()), Server),
            Err(DecodeError::InvalidProtobuf)
        );
    }
    // Unknown groups and duplicate singular messages retain protobuf merge
    // semantics rather than requiring a canonical encoding/field order.
    let bytes = [0xfb, 1, 8, 1, 0xfc, 1, 0x10, 1];
    assert_eq!(
        decode(&raw(&bytes, 0, bytes.len()), Server),
        Ok(StateBatch {
            initial_complete: true,
            records: vec![]
        })
    );
    let batch = publication(1);
    let mut record = batch.records[0].encode_to_vec();
    record.extend_from_slice(&[0x0a, 0]);
    let mut duplicate = vec![0x0a, u8::try_from(record.len()).unwrap()];
    duplicate.extend_from_slice(&record);
    assert_eq!(
        decode(&raw(&duplicate, 0, duplicate.len()), Client),
        Ok(batch)
    );
}

#[test]
fn independent_lz4_blocks_cover_long_literals_matches_and_overlap() {
    for length in [1, 15, 16, 255, 256, 1024, MAX_DECODED_SIZE] {
        let mut state = 7u32;
        let random: Vec<_> = (0..length)
            .map(|_| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                u8::try_from(state >> 24).unwrap()
            })
            .collect();
        for bytes in [
            vec![b'x'; length],
            (0..length)
                .map(|i| u8::try_from(i % 251).unwrap())
                .collect(),
            random,
        ] {
            assert_eq!(decode_lz4(&compress(&bytes), length), Ok(bytes));
        }
    }
}

fn rtp() -> RtpParameters {
    RtpParameters {
        mid: String::from("0"),
        codec: Some(RtpCodecParameters {
            payload_type: Some(0),
            rtx_payload_type: None,
            feedback: vec![4],
            format: Some(CodecFormat {
                codec: Some(codec_format::Codec::Opus(OpusParameters::default())),
            }),
        }),
        header_extensions: vec![
            RtpHeaderExtensionParameters { r#type: 1, id: 1 },
            RtpHeaderExtensionParameters { r#type: 4, id: 4 },
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
    assert_eq!(decode(&encode(&batch, Client).unwrap(), Client), Ok(batch));
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
                        format: Some(CodecFormat {
                            codec: Some(codec_format::Codec::Vp8(Vp8Parameters {})),
                        }),
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
    let batch = |confirmed| StateBatch {
        initial_complete: false,
        records: vec![StateRecord {
            key: Some(ResourceKey {
                key: Some(resource_key::Key::Owned(1)),
            }),
            state: Some(state_record::State::Confirmed(confirmed)),
        }],
    };
    let accepted = batch(confirmed.clone());
    assert_eq!(
        decode(&encode(&accepted, Server).unwrap(), Server),
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
        .push(RtpHeaderExtensionParameters { r#type: 2, id: 2 });
    binding.codec.as_mut().unwrap().format = Some(CodecFormat {
        codec: Some(codec_format::Codec::Vp9(Vp9Parameters { profile_id: None })),
    });
    binding.codec.as_mut().unwrap().feedback.push(1);
    let original = video(binding.clone());
    let mut explicit = binding.clone();
    explicit
        .codec
        .as_mut()
        .unwrap()
        .format
        .as_mut()
        .unwrap()
        .codec = Some(codec_format::Codec::Vp9(Vp9Parameters {
        profile_id: Some(0),
    }));
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
        |v| v.encodings[0].scalability_mode = Some(RtpScalabilityMode::L1t1 as i32),
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
    let opus = |dtx| CodecFormat {
        codec: Some(codec_format::Codec::Opus(OpusParameters {
            dtx,
            ..OpusParameters::default()
        })),
    };
    assert!(!validation::formats_equivalent(
        &opus(None),
        &opus(Some(false))
    ));
    let format = |parameters| CodecFormat {
        codec: Some(codec_format::Codec::Opus(parameters)),
    };
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
    let h264 = |asymmetry| CodecFormat {
        codec: Some(codec_format::Codec::H264(H264Parameters {
            profile_level_id: Some(0x42e01f),
            level_asymmetry_allowed: asymmetry,
        })),
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
    let default = CodecFormat {
        codec: Some(codec_format::Codec::Vp9(Vp9Parameters { profile_id: None })),
    };
    let explicit = CodecFormat {
        codec: Some(codec_format::Codec::Vp9(Vp9Parameters {
            profile_id: Some(0),
        })),
    };
    assert!(validation::formats_equivalent(&default, &explicit));
    let capability = |format| RtpCodecCapability {
        format: Some(format),
        feedback: vec![4],
        rtx_supported: false,
        scalability_modes: vec![],
    };
    let mut caps = RtpCapabilities {
        codecs: vec![capability(default)],
        header_extensions: vec![
            1,
            4,
            RtpHeaderExtensionType::RtpHeaderExtensionVideoLayersAllocation as i32,
        ],
        rtcp_modes: vec![1],
    };
    assert_eq!(validation::validate_capabilities(&caps), Ok(()));
    #[cfg(feature = "json")]
    {
        use serde_json::{from_str, from_value, json, to_value};

        let encoded = to_value(&caps).unwrap();
        assert_eq!(
            encoded,
            json!({
                "codecs": [{
                    "format": {"vp9": {}},
                    "feedback": ["RTCP_FEEDBACK_TRANSPORT_CC"]
                }],
                "header_extensions": [
                    "RTP_HEADER_EXTENSION_MID",
                    "RTP_HEADER_EXTENSION_TRANSPORT_CC_DRAFT_01",
                    "RTP_HEADER_EXTENSION_VIDEO_LAYERS_ALLOCATION"
                ],
                "rtcp_modes": ["RTCP_MODE_COMPOUND"]
            })
        );
        assert_eq!(
            from_value::<RtpCapabilities>(encoded.clone()).unwrap(),
            caps
        );
        let explicit_json = to_value(&explicit).unwrap();
        assert_eq!(explicit_json, json!({"vp9": {"profile_id": 0}}));
        assert_eq!(from_value::<CodecFormat>(explicit_json).unwrap(), explicit);
        let opus = OpusParameters {
            stereo: Some(false),
            source_stereo: Some(false),
            in_band_fec: Some(false),
            dtx: Some(false),
            constant_bitrate: Some(false),
            ..Default::default()
        };
        let opus_json = json!({
            "stereo": false, "source_stereo": false, "in_band_fec": false,
            "dtx": false, "constant_bitrate": false
        });
        assert_eq!(to_value(&opus).unwrap(), opus_json);
        assert_eq!(from_value::<OpusParameters>(opus_json).unwrap(), opus);
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
        assert!(from_str::<RtpCapabilities>(r#"{"rtcp_modes":[1],"rtcpModes":[2]}"#).is_err());
        assert!(from_str::<RtpCapabilities>(r#"{"unknown":true}"#).is_err());
        assert!(from_str::<RtpCapabilities>(r#"{"header_extensions":["UNKNOWN"]}"#).is_err());
        assert!(from_str::<RtpCapabilities>("{} {}").is_err());
        let mut unspecified_enum = encoded;
        unspecified_enum["header_extensions"][0] = json!(0);
        let parsed = from_value::<RtpCapabilities>(unspecified_enum).unwrap();
        assert_eq!(
            validation::validate_capabilities(&parsed),
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
    let unknown = Track {
        track_id: String::from("track"),
        participant_id: String::from("participant"),
        label: String::from("camera"),
        kind: Some(track::Kind::Video(VideoTrack {
            format: Some(CodecFormat::default()),
        })),
    };
    assert_eq!(validation::validate_track(&unknown), Ok(()));
    let mut missing = unknown;
    missing.kind = Some(track::Kind::Video(VideoTrack { format: None }));
    assert_eq!(
        validation::validate_track(&missing),
        Err(ValidationError::Format)
    );
}

#[test]
fn session_and_owned_absence_have_distinct_failure_and_direction_rules() {
    let confirmed = |key, state| StateBatch {
        initial_complete: false,
        records: vec![StateRecord {
            key: Some(ResourceKey { key: Some(key) }),
            state: Some(state_record::State::Confirmed(state)),
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
        encode(
            &confirmed(resource_key::Key::Session(true), session.clone()),
            Server
        )
        .is_ok()
    );
    session.evaluated_revision = 1;
    assert_eq!(
        encode(
            &confirmed(resource_key::Key::Session(true), session),
            Server
        ),
        Err(EncodeError::Validation(ValidationError::Confirmed))
    );
    let mut absent = ConfirmedState {
        value: Some(confirmed_state::Value::Absent(AbsentState {
            reason: AbsenceReason::CreateRejected as i32,
        })),
        ..ConfirmedState::default()
    };
    assert!(
        encode(
            &confirmed(resource_key::Key::Owned(1), absent.clone()),
            Server
        )
        .is_err()
    );
    absent.rejection = Some(Failure {
        code: FailureCode::InvalidConfiguration as i32,
        diagnostic: String::new(),
    });
    let batch = confirmed(resource_key::Key::Owned(1), absent);
    assert_eq!(decode(&encode(&batch, Server).unwrap(), Server), Ok(batch));
    let terminal = ConfirmedState {
        value: Some(confirmed_state::Value::Session(SessionState {
            closed: Some(CloseReason::AuthorizationRevoked as i32),
            diagnostic: String::from("Grant revoked"),
            ..grant
        })),
        ..ConfirmedState::default()
    };
    let batch = confirmed(resource_key::Key::Session(true), terminal);
    assert_eq!(decode(&encode(&batch, Server).unwrap(), Server), Ok(batch));
}
