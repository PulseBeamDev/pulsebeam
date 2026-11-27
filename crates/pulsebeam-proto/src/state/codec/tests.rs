#![allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::unwrap_used
)]

use alloc::{boxed::Box, string::String, vec};

use super::*;

#[test]
fn native_clock_control_pins_direction_and_a_bounded_standalone_image() {
    use crate::state::{NativeClockChallenge, NativeClockExchange, native_clock_exchange::Message};
    let frame = NativeClockControl {
        exchange: Some(NativeClockExchange {
            owned: 7,
            clock_epoch: 2,
            message: Some(Message::Challenge(NativeClockChallenge { nonce: 11 })),
        }),
    };
    let wire = [0x32, 8, 8, 7, 16, 2, 0x22, 2, 8, 11];
    assert_eq!(
        encode_clock_control(&frame, ClockDirection::ServerToClient).unwrap(),
        wire
    );
    assert_eq!(
        decode_clock_control_frame(&wire, ClockDirection::ServerToClient),
        Some(Ok(frame))
    );
    assert_eq!(
        decode_clock_control_frame(&wire, ClockDirection::ClientToServer),
        Some(Err(DecodeError::Structure))
    );
    assert!(decode_client(&wire).is_err());
    assert!(decode_server(&wire).is_err());
    assert!(decode_data_control_frame(&wire).is_none());
    let mut oversized = [0; 65];
    oversized[0] = 0x32;
    assert_eq!(
        decode_clock_control_frame(&oversized, ClockDirection::ServerToClient),
        Some(Err(DecodeError::FrameSize))
    );
}
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
                                track_id: String::from("cursor"),
                                kind: Some(publication_spec::Kind::Data(DataPublication {
                                    capture_clock: None,
                                    delivery: DataDelivery::Lossy as i32,
                                    binding: Some(TrackBinding {
                                        transport: Some(track_binding::Transport::SctpDataV1(
                                            SctpDataV1Parameters {
                                                stream_id: Some(1),
                                                version: u32::from(crate::data_endpoint::DATA_V1),
                                                channel_incarnation: 1,
                                                delivery: DataV1Delivery::Unreliable as i32,
                                            },
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
        ..Default::default()
    }
}

#[test]
fn native_data_control_is_an_explicit_bounded_message_family() {
    use crate::data_control::{
        self, PublisherBinding, PublisherObservation, SourceUnit, publisher_observation,
    };
    use core::num::NonZeroU64;

    let binding = PublisherBinding {
        source: NonZeroU64::MIN,
    };
    let admission = PublisherObservation {
        record: Some(publisher_observation::Record::Admission(SourceUnit {
            source_incarnation: 1,
            unit_seq: Some(0),
        })),
    };
    let carrier = NativeDataControl {
        endpoint: Some(native_data_control::Endpoint::Publisher(OwnedDataControl {
            owned: 7,
            record: data_control::encode(binding, &admission).unwrap(),
        })),
    };
    let bytes = encode_data_control(&carrier).unwrap();
    assert_eq!(&bytes[..6], &[0x22, 24, 0x08, 7, 0x12, 20]);
    assert_eq!(decode_server(&bytes), Err(DecodeError::Structure));
    let NativeFrame::Data(decoded) = decode_server_frame(&bytes).unwrap() else {
        panic!("explicit DATA control carrier")
    };
    assert_eq!(decoded, carrier);
    let Some(native_data_control::Endpoint::Publisher(owned)) = decoded.endpoint else {
        panic!("publisher envelope")
    };
    assert_eq!(data_control::decode(binding, &owned.record), Ok(admission));
    assert_eq!(
        data_control::decode::<PublisherObservation>(
            PublisherBinding {
                source: NonZeroU64::MAX
            },
            &owned.record
        ),
        Err(data_control::Error::Binding)
    );

    let state = publication(1);
    let state_bytes = encode_client(&state).unwrap();
    assert_eq!(
        decode_client_frame(&state_bytes),
        Ok(NativeFrame::State(state))
    );
    for invalid in [
        [bytes.as_slice(), bytes.as_slice()].concat(),
        [bytes.as_slice(), state_bytes.as_slice()].concat(),
        [state_bytes.as_slice(), bytes.as_slice()].concat(),
        [[0xa2, 0].as_slice(), &bytes[1..]].concat(),
    ] {
        assert_eq!(decode_client_frame(&invalid), Err(DecodeError::Structure));
    }
    let frame = |owned, len| NativeDataControl {
        endpoint: Some(native_data_control::Endpoint::Subscriber(
            OwnedDataControl {
                owned,
                record: vec![7; len],
            },
        )),
    };
    let max = frame(u64::MAX, MAX_DATA_CONTROL);
    let maximal_bytes = encode_data_control(&max).unwrap();
    assert_eq!(maximal_bytes.first(), Some(&0x2a));
    assert_eq!(maximal_bytes.len(), MAX_DATA_CONTROL_FRAME);
    assert_eq!(decode_client(&maximal_bytes), Err(DecodeError::Structure));
    assert_eq!(
        decode_client_frame(&maximal_bytes),
        Ok(NativeFrame::Data(max))
    );
    for bad in [frame(0, 1), frame(1, 0), frame(1, MAX_DATA_CONTROL + 1)] {
        assert_eq!(
            encode_data_control(&bad),
            Err(EncodeError::Validation(ValidationError::Batch))
        );
    }
    assert_eq!(
        decode_client_frame(&frame(u64::MAX, MAX_DATA_CONTROL + 1).encode_to_vec()),
        Err(DecodeError::FrameSize)
    );
}

#[test]
fn data_v1_requires_its_exact_negotiated_transport_variant() {
    fn batch(parameters: SctpDataV1Parameters, delivery: DataDelivery) -> ClientStateBatch {
        let mut batch = publication(1);
        let Some(desired_state::Value::Present(spec)) = desired(&mut batch).value.as_mut() else {
            panic!("publication fixture")
        };
        let Some(resource_spec::Declaration::Publication(publication)) = spec.declaration.as_mut()
        else {
            panic!("publication fixture")
        };
        let Some(publication_spec::Kind::Data(data)) = publication.kind.as_mut() else {
            panic!("DATA fixture")
        };
        data.delivery = delivery as i32;
        data.binding = Some(TrackBinding {
            transport: Some(track_binding::Transport::SctpDataV1(parameters)),
        });
        batch
    }
    fn validate(mut batch: ClientStateBatch) -> Result<(), ValidationError> {
        let Some(desired_state::Value::Present(spec)) = desired(&mut batch).value.as_ref() else {
            panic!("publication fixture")
        };
        validation::validate_spec(spec)
    }
    let fmt = SctpDataV1Parameters {
        stream_id: Some(7),
        version: u32::from(crate::data_endpoint::DATA_V1),
        channel_incarnation: u64::MAX,
        delivery: DataV1Delivery::Unreliable as i32,
    };
    let requested = batch(fmt, DataDelivery::Lossy);
    assert_eq!(validate(requested.clone()), Ok(()));
    let bytes = encode_client(&requested).unwrap();
    assert_eq!(decode_client(&bytes), Ok(requested));
    assert_ne!(bytes, encode_client(&publication(1)).unwrap());
    for rejected in [
        SctpDataV1Parameters { version: 0, ..fmt },
        SctpDataV1Parameters { version: 2, ..fmt },
        SctpDataV1Parameters {
            channel_incarnation: 0,
            ..fmt
        },
        SctpDataV1Parameters {
            stream_id: Some(0),
            ..fmt
        },
        SctpDataV1Parameters {
            delivery: DataV1Delivery::Unspecified as i32,
            ..fmt
        },
        SctpDataV1Parameters {
            delivery: DataV1Delivery::Latest as i32,
            ..fmt
        },
    ] {
        assert_eq!(
            validate(batch(rejected, DataDelivery::Lossy)),
            Err(ValidationError::Binding)
        );
    }
    assert_eq!(
        validate(batch(fmt, DataDelivery::Ordered)),
        Err(ValidationError::Binding)
    );
}

#[test]
fn data_bindings_are_scoped_to_their_carrier_and_reject_retired_sctp() {
    use track_binding::Transport;
    let mut batch = publication(1);
    let Some(desired_state::Value::Present(spec)) = desired(&mut batch).value.as_mut() else {
        panic!("publication fixture")
    };
    let Some(resource_spec::Declaration::Publication(publication)) = spec.declaration.as_mut()
    else {
        panic!("publication fixture")
    };
    let Some(publication_spec::Kind::Data(data)) = publication.kind.as_mut() else {
        panic!("DATA fixture")
    };
    let binding = TrackBinding {
        transport: Some(Transport::QuicDataV1(QuicDataV1Parameters {
            binding_id: 7,
            version: 1,
            incarnation: 9,
            delivery: DataV1Delivery::Unreliable as i32,
            record_dd: false,
        })),
    };
    let quic_bytes = binding.encode_to_vec();
    assert_eq!(
        quic_bytes,
        [0x22, 15, 8, 7, 16, 1, 25, 9, 0, 0, 0, 0, 0, 0, 0, 32, 1]
    );
    let retired = TrackBinding::decode(&[0x12, 2, 8, 1][..]).unwrap();
    assert!(retired.transport.is_none());
    let active = data.binding.replace(retired);
    assert_eq!(
        validation::validate_spec(spec),
        Err(ValidationError::Binding)
    );
    let Some(resource_spec::Declaration::Publication(publication)) = spec.declaration.as_mut()
    else {
        panic!("publication fixture")
    };
    let Some(publication_spec::Kind::Data(data)) = publication.kind.as_mut() else {
        panic!("DATA fixture")
    };
    data.binding = active;
    assert_eq!(validation::validate_spec(spec), Ok(()));
    assert_eq!(
        validation::validate_spec_for(spec, validation::BindingProfile::Quic),
        Err(ValidationError::Binding)
    );
    let Some(resource_spec::Declaration::Publication(publication)) = spec.declaration.as_mut()
    else {
        panic!("publication fixture")
    };
    let Some(publication_spec::Kind::Data(data)) = publication.kind.as_mut() else {
        panic!("DATA fixture")
    };
    let mut capable = binding;
    let Some(Transport::QuicDataV1(parameters)) = capable.transport.as_mut() else {
        panic!("QUIC DATA binding fixture");
    };
    parameters.record_dd = true;
    assert_eq!(
        capable.encode_to_vec(),
        [
            0x22, 17, 8, 7, 16, 1, 25, 9, 0, 0, 0, 0, 0, 0, 0, 32, 1, 40, 1
        ]
    );
    data.binding = Some(capable);
    assert_eq!(
        validation::validate_spec_for(spec, validation::BindingProfile::Quic),
        Ok(())
    );
    assert_eq!(
        validation::validate_spec(spec),
        Err(ValidationError::Binding)
    );
    let encoded = encode_quic_client(&batch).unwrap();
    assert_eq!(decode_quic_client(&encoded), Ok(batch));
}

#[test]
fn quic_rtp_binding_compiles_opus_coordinates_without_rtc_fields() {
    use validation::QuicRtpBinding;

    let mut parameters = QuicRtpParameters {
        binding_id: 7,
        incarnation: 9,
        primary_ssrc: 11,
        video_dd: None,
        codec: Some(RtpCodecParameters {
            payload_type: Some(111),
            format: Some(CodecFormat::from(codec_format::Codec::Opus(
                OpusParameters::default(),
            ))),
            ..Default::default()
        }),
    };
    let binding = QuicRtpBinding::compile(&parameters, TrackKind::Audio).unwrap();
    assert_eq!(binding.id().get(), 7);
    assert_eq!(binding.incarnation().get(), 9);
    assert_eq!(binding.ssrc().get(), 11);
    assert_eq!(binding.payload_type(), 111);
    assert_eq!(binding.format().clock_rate, 48_000);
    assert!(binding.feedback().is_empty());
    assert_eq!(
        QuicRtpBinding::compile(&parameters, TrackKind::Video),
        Err(ValidationError::Format)
    );

    parameters.video_dd = Some(QuicVideoDd::Emit as i32);
    assert_eq!(
        QuicRtpBinding::compile(&parameters, TrackKind::Audio),
        Err(ValidationError::Binding)
    );
    parameters.video_dd = None;
    parameters.codec.as_mut().unwrap().payload_type = Some(72);
    assert_eq!(
        QuicRtpBinding::compile(&parameters, TrackKind::Audio),
        Err(ValidationError::Binding)
    );
}

#[test]
fn h264_requires_an_explicit_directional_video_dd_contract() {
    use validation::QuicRtpBinding;

    let mut parameters = QuicRtpParameters {
        binding_id: 7,
        incarnation: 9,
        primary_ssrc: 11,
        video_dd: None,
        codec: Some(RtpCodecParameters {
            payload_type: Some(102),
            format: Some(CodecFormat::from(codec_format::Codec::H264(
                H264Parameters::default(),
            ))),
            ..Default::default()
        }),
    };
    assert!(QuicRtpBinding::compile(&parameters, TrackKind::Video).is_err());
    for (declared, publish, subscribe) in [
        (QuicVideoDd::Absent, true, true),
        (QuicVideoDd::Emit, true, false),
        (QuicVideoDd::Accept, false, true),
        (QuicVideoDd::Require, false, true),
    ] {
        parameters.video_dd = Some(declared as i32);
        let binding = QuicRtpBinding::compile(&parameters, TrackKind::Video).unwrap();
        assert_eq!(binding.validate_video_dd_direction(true).is_ok(), publish);
        assert_eq!(
            binding.validate_video_dd_direction(false).is_ok(),
            subscribe
        );
    }
    for unsupported in [0, 127] {
        parameters.video_dd = Some(unsupported);
        assert!(QuicRtpBinding::compile(&parameters, TrackKind::Video).is_err());
    }
}

#[test]
fn installed_data_v1_publication_requires_the_authenticated_source_incarnation() {
    let mut desired_batch = publication(1);
    let Some(desired_state::Value::Present(spec)) = desired(&mut desired_batch).value.as_mut()
    else {
        panic!("publication fixture");
    };
    spec.settings.as_mut().unwrap().enabled = Some(true);
    let source = data_source();
    let mut live = LiveResource {
        spec: Some(spec.as_ref().clone()),
        participant: Some(Participant {
            participant_id: source.participant_id.clone(),
            room_id: source.room_id.clone(),
        }),
        track: Some(source),
        execution: ExecutionStatus::Installed as i32,
        ..Default::default()
    };
    live.data_v1_source = Some(u64::MAX);
    assert_eq!(validation::validate_live(&live), Ok(()));
    for source in [None, Some(0)] {
        live.data_v1_source = source;
        assert_eq!(
            validation::validate_live(&live),
            Err(ValidationError::Binding)
        );
    }
}

fn desired(batch: &mut ClientStateBatch) -> &mut DesiredState {
    batch.records[0].desired.as_mut().unwrap()
}

fn data_source() -> Track {
    Track {
        publication_id: String::from("replacement-track"),
        participant_id: String::from("alice"),
        room_id: String::from("room"),
        track_id: String::from("cursor"),
        kind: Some(track::Kind::Data(DataTrack {
            capture_clock: None,
            delivery: DataDelivery::Lossy as i32,
            record_dd: false,
        })),
    }
}

#[test]
fn logical_subscription_accepts_only_an_enabled_exact_source_offer() {
    let spec = ResourceSpec {
        settings: Some(ResourceSettings {
            enabled: Some(true),
            receive: None,
        }),
        declaration: Some(resource_spec::Declaration::LogicalSubscription(
            LogicalSubscriptionSpec {
                source: Some(LogicalSourceAddress {
                    room: String::from("room"),
                    participant_id: String::from("alice"),
                    kind: TrackKind::Data as i32,
                    track_id: String::from("cursor"),
                }),
            },
        )),
    };
    let mut subscription = LogicalSubscriptionState {
        data_v1: None,
        spec: Some(spec),
        source_revision: 0,
        source: None,
        execution: None,
    };
    assert_eq!(
        validation::validate_logical_subscription(&subscription),
        Ok(())
    );

    subscription.source = Some(data_source());
    assert_eq!(
        validation::validate_logical_subscription(&subscription),
        Err(ValidationError::Confirmed)
    );
    subscription.source_revision = (1 << 53) + 1;
    assert_eq!(
        validation::validate_logical_subscription(&subscription),
        Err(ValidationError::Binding),
        "DATA needs an offer as soon as a source is present",
    );
    subscription.data_v1 = Some(DataV1ReceiverOffer {
        source: u64::MAX,
        branch: (1 << 53) + 1,
        capture_clock: None,
    });
    let binding = TrackBinding {
        transport: Some(track_binding::Transport::SctpDataV1(SctpDataV1Parameters {
            stream_id: Some(5),
            version: u32::from(crate::data_endpoint::DATA_V1),
            channel_incarnation: 9,
            delivery: DataV1Delivery::Unreliable as i32,
        })),
    };
    let mut report = LogicalReceiverReport {
        source_revision: 0,
        outcome: Some(logical_receiver_report::Outcome::Prepared(binding.clone())),
    };
    let proposal = |report| ClientStateBatch {
        records: vec![ClientStateRecord {
            owned: 1,
            desired: None,
            execution: Some(ExecutionReport {
                revision: 2,
                outcome: Some(execution_report::Outcome::LogicalReceiver(report)),
            }),
        }],
        ..Default::default()
    };
    assert!(encode_client(&proposal(report.clone())).is_err());
    report.source_revision = subscription.source_revision;
    let client = proposal(report);
    assert_eq!(decode_client(&encode_client(&client).unwrap()), Ok(client));

    subscription.execution = Some(logical_subscription_state::Execution::Installed(binding));
    let server = ServerStateBatch {
        records: vec![ServerStateRecord {
            key: Some(ResourceKey {
                key: Some(resource_key::Key::Owned(1)),
            }),
            confirmed: Some(ConfirmedState {
                evaluated_revision: 1,
                value: Some(confirmed_state::Value::LogicalSubscription(
                    subscription.clone().into(),
                )),
                ..Default::default()
            }),
        }],
        ..Default::default()
    };
    assert_eq!(
        decode_server(&encode_server(&server).unwrap()),
        Ok(server.clone())
    );

    for (delivery, expected) in [
        (DataV1Delivery::Unreliable, Ok(())),
        (DataV1Delivery::Latest, Err(ValidationError::Binding)),
    ] {
        let mut data_v1 = subscription.clone();
        data_v1.data_v1 = None;
        data_v1.execution = Some(logical_subscription_state::Execution::Installed(
            TrackBinding {
                transport: Some(track_binding::Transport::SctpDataV1(SctpDataV1Parameters {
                    stream_id: Some(5),
                    version: u32::from(crate::data_endpoint::DATA_V1),
                    channel_incarnation: 9,
                    delivery: delivery as i32,
                })),
            },
        ));
        assert_eq!(
            validation::validate_logical_subscription(&data_v1),
            Err(ValidationError::Binding)
        );
        data_v1.data_v1 = Some(DataV1ReceiverOffer {
            source: u64::MAX,
            branch: (1 << 53) + 1,
            capture_clock: Some(DataV1CaptureClock {
                id: String::from("sender-clock"),
                epoch: 19,
            }),
        });
        assert_eq!(
            validation::validate_logical_subscription(&data_v1),
            Err(ValidationError::Binding),
            "a receiver offer cannot change the source clock",
        );
        let Some(track::Kind::Data(track)) = data_v1
            .source
            .as_mut()
            .and_then(|source| source.kind.as_mut())
        else {
            panic!("DATA source")
        };
        track.capture_clock = data_v1.data_v1.as_ref().unwrap().capture_clock.clone();
        assert_eq!(
            validation::validate_logical_subscription(&data_v1),
            expected
        );
        data_v1.execution = Some(logical_subscription_state::Execution::Installed(
            TrackBinding { transport: None },
        ));
        assert_eq!(
            validation::validate_logical_subscription(&data_v1),
            Err(ValidationError::Binding)
        );
        data_v1.execution = None;
        assert_eq!(validation::validate_logical_subscription(&data_v1), Ok(()));
        data_v1.data_v1.as_mut().unwrap().branch = 0;
        assert_eq!(
            validation::validate_logical_subscription(&data_v1),
            Err(ValidationError::Binding)
        );
    }

    let mut with_dd = server;
    let Some(confirmed_state::Value::LogicalSubscription(state)) = with_dd.records[0]
        .confirmed
        .as_mut()
        .unwrap()
        .value
        .as_mut()
    else {
        panic!("logical subscription fixture");
    };
    let Some(track::Kind::Data(source)) = state
        .source
        .as_mut()
        .and_then(|source| source.kind.as_mut())
    else {
        panic!("DATA fixture");
    };
    source.record_dd = true;
    assert_eq!(
        validation::validate_logical_subscription(state),
        Err(ValidationError::Binding)
    );
    let mut quic = QuicDataV1Parameters {
        binding_id: 5,
        version: u32::from(crate::data_endpoint::DATA_V1),
        incarnation: 9,
        delivery: DataV1Delivery::Unreliable as i32,
        record_dd: false,
    };
    let mut quic_batch = |quic| {
        let Some(confirmed_state::Value::LogicalSubscription(state)) = with_dd.records[0]
            .confirmed
            .as_mut()
            .unwrap()
            .value
            .as_mut()
        else {
            panic!("logical subscription fixture");
        };
        state.execution = Some(logical_subscription_state::Execution::Installed(
            TrackBinding {
                transport: Some(track_binding::Transport::QuicDataV1(quic)),
            },
        ));
        encode_quic_server(&with_dd)
    };
    assert_eq!(
        quic_batch(quic),
        Err(EncodeError::Validation(ValidationError::Binding))
    );
    quic.record_dd = true;
    assert!(quic_batch(quic).is_ok());

    let mut missing_source = subscription.clone();
    missing_source.source = None;
    assert_eq!(
        validation::validate_logical_subscription(&missing_source),
        Err(ValidationError::Confirmed)
    );
    subscription.source.as_mut().unwrap().track_id = String::from("other");
    assert_eq!(
        validation::validate_logical_subscription(&subscription),
        Err(ValidationError::Identity)
    );
    subscription
        .spec
        .as_mut()
        .unwrap()
        .settings
        .as_mut()
        .unwrap()
        .enabled = Some(false);
    assert_eq!(
        validation::validate_logical_subscription(&subscription),
        Err(ValidationError::Confirmed)
    );
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
        ..Default::default()
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
        ..Default::default()
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
        ..Default::default()
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
            records: vec![],
            ..Default::default()
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
        extmap_allow_mixed: Some(true),
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
        track_id: String::from("mic"),
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
            publication_id: String::from("opaque-track"),
            kind: TrackKind::Video as i32,
            source_room: String::from("room"),
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
    for decode_target in [0, 31] {
        let explicit = VideoReceiveOptions {
            decode_target: Some(decode_target),
            ..defaults.clone()
        };
        assert_eq!(validation::validate_spec(&spec(explicit)), Ok(()));
    }
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
            decode_target: Some(32),
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
        data_v1_source: None,
        spec: Some(*spec),
        participant: Some(Participant {
            participant_id: String::from("participant"),
            room_id: String::from("room"),
        }),
        track: Some(Track {
            publication_id: String::from("track"),
            participant_id: String::from("participant"),
            room_id: String::from("room"),
            track_id: String::from("cursor"),
            kind: Some(track::Kind::Data(DataTrack {
                capture_clock: None,
                delivery: DataDelivery::Lossy as i32,
                record_dd: false,
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
        ..Default::default()
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
    invalid.data_v1_source = Some(1);
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
            track_id: String::from("camera"),
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
    let subscription = |publication_id: &str| ResourceSpec {
        settings: Some(ResourceSettings {
            enabled: Some(true),
            receive: Some(resource_settings::Receive::Video(
                VideoReceiveOptions::default(),
            )),
        }),
        declaration: Some(resource_spec::Declaration::Subscription(SubscriptionSpec {
            publication_id: String::from(publication_id),
            kind: TrackKind::Video as i32,
            source_room: String::from("room"),
            binding: Some(TrackBinding {
                transport: Some(track_binding::Transport::Rtp(binding.clone())),
            }),
        })),
    };
    assert!(!validation::declarations_equivalent(
        &subscription("source-a"),
        &subscription("source-b")
    ));
    let mut moved_room = subscription("source-a");
    {
        let Some(resource_spec::Declaration::Subscription(spec)) = moved_room.declaration.as_mut()
        else {
            panic!("subscription")
        };
        spec.source_room = String::from("other-room");
    }
    assert!(!validation::declarations_equivalent(
        &subscription("source-a"),
        &moved_room
    ));
    assert_eq!(validation::validate_spec(&moved_room), Ok(()));
    if let Some(resource_spec::Declaration::Subscription(spec)) = moved_room.declaration.as_mut() {
        spec.source_room.clear();
    }
    assert_eq!(
        validation::validate_spec(&moved_room),
        Err(ValidationError::Identity)
    );
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
    let Some(track_binding::Transport::SctpDataV1(sctp)) =
        d.binding.as_mut().unwrap().transport.as_mut()
    else {
        panic!("sctp")
    };
    sctp.stream_id = Some(2);
    assert!(!validation::declarations_equivalent(data, &moved));
}

#[test]
fn mutable_defaults_do_not_erase_codec_parameter_presence() {
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
    options.decode_target = Some(0);
    assert!(!validation::settings_equivalent(
        &default,
        &settings(options.clone())
    ));
    options.decode_target = None;
    options.priority = Some(2);
    assert!(!validation::settings_equivalent(
        &default,
        &settings(options.clone())
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
        publication_id: String::from("track"),
        participant_id: String::from("participant"),
        room_id: String::from("room"),
        track_id: String::from("camera"),
        kind: Some(track::Kind::Video(VideoTrack {
            av1_whole_source: false,
            format: Some(future_format.clone()),
        })),
    };
    assert_eq!(validation::validate_track(&unknown), Ok(()));
    let mut whole = unknown.clone();
    let Some(track::Kind::Video(video)) = whole.kind.as_mut() else {
        panic!("video source");
    };
    video.av1_whole_source = true;
    assert_eq!(
        validation::validate_track(&whole),
        Err(ValidationError::Format)
    );
    whole.kind = Some(track::Kind::Video(VideoTrack {
        format: CodecFormat::native_quic_baseline("video/av1"),
        av1_whole_source: true,
    }));
    assert_eq!(validation::validate_track(&whole), Ok(()));
    caps.codecs.pop();
    caps.codecs.push(capability(future_format));
    caps.header_extensions
        .push("urn:future:optional-extension".into());
    caps.codecs[0].feedback.push("future-feedback".into());
    assert_eq!(validation::validate_capabilities(&caps), Ok(()));
    let mut missing = unknown;
    missing.kind = Some(track::Kind::Video(VideoTrack::default()));
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
        ..Default::default()
    };
    let grant = SessionState {
        authorization: Some(Authorization {
            expires_at_unix_seconds: i64::MAX,
            revision: u64::MAX,
        }),
        closed: None,
        diagnostic: String::new(),
        successor_receipt: None,
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
fn selector_slices_bound_geometry_and_validate_membership() {
    let mut spec = ResourceSpec {
        declaration: Some(resource_spec::Declaration::SubscriptionSelector(
            SubscriptionSelectorSpec {
                kind: Some(TrackKind::Video as i32),
                ..Default::default()
            },
        )),
        settings: Some(ResourceSettings {
            enabled: Some(true),
            receive: None,
        }),
    };
    let batch = |spec: &ResourceSpec, status: SelectorMatchStatus, offset, total, matches| {
        ServerStateBatch {
            records: vec![ServerStateRecord {
                key: Some(ResourceKey {
                    key: Some(resource_key::Key::Owned(u64::MAX)),
                }),
                confirmed: Some(ConfirmedState {
                    evaluated_revision: u64::MAX,
                    value: Some(confirmed_state::Value::SubscriptionSelector(Box::new(
                        SubscriptionSelectorState {
                            spec: Some(spec.clone()),
                            status: status as i32,
                            membership: (status != SelectorMatchStatus::Pending).then_some(
                                SelectorMembershipSpan {
                                    revision: u64::MAX,
                                    offset,
                                    total,
                                },
                            ),
                            matches,
                        },
                    ))),
                    rejection: None,
                }),
            }],
            ..Default::default()
        }
    };
    let addresses = |count| {
        (0..count)
            .map(|index| LogicalSourceAddress {
                room: "r".repeat(36),
                participant_id: "p".repeat(36),
                kind: TrackKind::Video as i32,
                track_id: alloc::format!("video{index:031}"),
            })
            .collect::<alloc::vec::Vec<_>>()
    };
    let complete = SelectorMatchStatus::Complete;
    let total = validation::MAX_SELECTOR_MATCHES;
    let slice = validation::MAX_SELECTOR_SLICE_MATCHES;
    let wire_total = u32::try_from(total).unwrap();
    for (index, addresses) in addresses(total).chunks(slice).enumerate() {
        let chunk = batch(
            &spec,
            complete,
            u32::try_from(index * slice).unwrap(),
            wire_total,
            addresses.to_vec(),
        );
        let frame = encode_server(&chunk).unwrap();
        assert!(frame.len() <= MAX_FRAME_SIZE);
        assert_eq!(decode_server(&frame), Ok(chunk));
    }
    assert!(encode_server(&batch(&spec, complete, 0, wire_total + 1, addresses(1))).is_err());
    assert!(encode_server(&batch(&spec, complete, 0, wire_total, addresses(slice + 1))).is_err());
    assert!(encode_server(&batch(&spec, complete, 3, 3, addresses(1))).is_err());
    assert!(encode_server(&batch(&spec, complete, 0, 1, addresses(0))).is_err());
    let mut duplicate = addresses(1);
    duplicate.push(duplicate[0].clone());
    assert!(encode_server(&batch(&spec, complete, 0, 2, duplicate)).is_err());
    let mut mismatch = addresses(1);
    mismatch[0].kind = TrackKind::Audio as i32;
    assert!(encode_server(&batch(&spec, complete, 0, 1, mismatch)).is_err());
    let pending = SelectorMatchStatus::Pending;
    assert!(encode_server(&batch(&spec, pending, 0, 0, addresses(0))).is_ok());
    assert!(encode_server(&batch(&spec, pending, 0, 1, addresses(1))).is_err());
    assert!(
        encode_server(&batch(
            &spec,
            SelectorMatchStatus::Overflow,
            0,
            1,
            addresses(1)
        ))
        .is_ok()
    );
    let Some(resource_spec::Declaration::SubscriptionSelector(selector)) =
        spec.declaration.as_mut()
    else {
        panic!("expected subscription selector declaration");
    };
    selector.room = Some("room*".into());
    assert_eq!(
        validation::validate_spec(&spec),
        Err(ValidationError::Identity)
    );
}

#[test]
fn successor_receipt_round_trips_only_on_live_sessions_and_redacts_debug() {
    let mut receipt = SuccessorReceipt {
        proof: vec![0xA5; MAX_SUCCESSOR_RECEIPT_BYTES],
        expires_at_unix_seconds: 1300,
    };
    let batch = |receipt, closed| ServerStateBatch {
        records: vec![ServerStateRecord {
            key: Some(ResourceKey {
                key: Some(resource_key::Key::Session(true)),
            }),
            confirmed: Some(ConfirmedState {
                value: Some(confirmed_state::Value::Session(SessionState {
                    authorization: Some(Authorization {
                        expires_at_unix_seconds: 2000,
                        revision: 1,
                    }),
                    successor_receipt: Some(receipt),
                    closed,
                    ..SessionState::default()
                })),
                ..ConfirmedState::default()
            }),
        }],
        ..Default::default()
    };
    assert_eq!(
        alloc::format!("{receipt:?}"),
        "SuccessorReceipt([redacted])"
    );
    let live = batch(receipt.clone(), None);
    assert_eq!(decode_server(&encode_server(&live).unwrap()), Ok(live));
    assert!(
        encode_server(&batch(
            receipt.clone(),
            Some(CloseReason::ParticipantReplaced as i32)
        ))
        .is_err()
    );
    receipt.proof.push(0);
    assert!(encode_server(&batch(receipt, None)).is_err());
}

#[test]
fn execution_failures_are_required_scoped_and_independently_revisioned() {
    let mut batch = ClientStateBatch {
        records: vec![ClientStateRecord {
            owned: 1,
            desired: None,
            execution: Some(ExecutionReport {
                revision: 2,
                outcome: Some(execution_report::Outcome::Failure(Failure {
                    code: FailureCode::InternalFailure as i32,
                    diagnostic: String::from("receiver failed after admission"),
                })),
            }),
        }],
        ..Default::default()
    };
    assert_eq!(
        decode_client(&encode_client(&batch).unwrap()),
        Ok(batch.clone())
    );
    batch.records[0].execution.as_mut().unwrap().outcome = None;
    assert_eq!(
        encode_client(&batch),
        Err(EncodeError::Validation(ValidationError::Failure))
    );
    batch.records[0].execution.as_mut().unwrap().outcome =
        Some(execution_report::Outcome::Failure(Failure {
            code: FailureCode::InternalFailure as i32,
            diagnostic: String::new(),
        }));
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
