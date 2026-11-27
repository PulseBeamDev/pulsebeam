use super::*;

#[test]
fn logical_receiver_reports_follow_the_owned_offer_not_the_publication() {
    use wire::logical_receiver_report::Outcome;

    let mut client = NativeState::new(64 * 1024, participant()).unwrap();
    client
        .apply(&frame(vec![session(1, 2_000_000_000, None)], true))
        .unwrap();
    let baseline = client.retained_bytes;
    let selector = TrackSelector {
        room: "room".into(),
        participant_id: "peer".into(),
        track_id: "changes".into(),
    };
    let resource = Resource::LogicalData(selector.clone());
    let spec = wire::ResourceSpec {
        settings: Some(wire::ResourceSettings {
            enabled: Some(true),
            receive: None,
        }),
        declaration: Some(wire::resource_spec::Declaration::LogicalSubscription(
            wire::LogicalSubscriptionSpec {
                source: Some(wire::LogicalSourceAddress {
                    room: selector.room.clone(),
                    participant_id: selector.participant_id.clone(),
                    kind: wire::TrackKind::Data as i32,
                    track_id: selector.track_id.clone(),
                }),
            },
        )),
    };
    let confirming = |source_revision, source: Option<&str>| {
        record(
            1,
            wire::ConfirmedState {
                evaluated_revision: 1,
                rejection: None,
                value: Some(wire::confirmed_state::Value::LogicalSubscription(Box::new(
                    wire::LogicalSubscriptionState {
                        data_v1: source.map(|_| wire::DataV1ReceiverOffer {
                            source: source_revision,
                            branch: source_revision,
                            capture_clock: None,
                        }),
                        spec: Some(spec.clone()),
                        source_revision,
                        source: source.map(|id| wire::Track {
                            publication_id: id.into(),
                            room_id: selector.room.clone(),
                            participant_id: selector.participant_id.clone(),
                            track_id: selector.track_id.clone(),
                            kind: Some(wire::track::Kind::Data(wire::DataTrack {
                                capture_clock: None,
                                delivery: wire::DataDelivery::Ordered as i32,
                                record_dd: false,
                            })),
                        }),
                        execution: None,
                    },
                ))),
            },
        )
    };
    let binding = wire::TrackBinding {
        transport: Some(wire::track_binding::Transport::SctpDataV1(
            wire::SctpDataV1Parameters {
                stream_id: Some(9),
                version: u32::from(pulsebeam_proto::data_endpoint::DATA_V1),
                channel_incarnation: 9,
                delivery: wire::DataV1Delivery::Reliable as i32,
            },
        )),
    };

    client.stage(resource.clone(), Some(spec.clone())).unwrap();
    assert_eq!(next(&mut client)[0].0, 1);
    client
        .apply(&frame(vec![confirming(0, None)], false))
        .unwrap();
    assert!(client.logical_offer(&resource).is_none());
    assert!(client.next_frame().unwrap().is_none());

    client
        .apply(&frame(vec![confirming(1, Some("old"))], false))
        .unwrap();
    let (old, _) = client.logical_offer(&resource).unwrap();
    assert_eq!(old.reference, 1);
    client
        .report_logical_receiver(&resource, old, Outcome::Prepared(binding.clone()))
        .unwrap();
    client
        .apply(&frame(vec![confirming(3, Some("new"))], false))
        .unwrap();
    assert!(
        client.next_frame().unwrap().is_none(),
        "skip queued stale work"
    );
    assert!(client.source("old").is_none());
    assert!(client.source("new").is_some());
    assert!(client.tracks().is_empty());
    assert!(
        client
            .apply(&frame(vec![confirming(2, Some("old"))], false))
            .is_err()
    );
    assert!(
        client
            .apply(&frame(vec![confirming(3, Some("old"))], false))
            .is_err()
    );
    client
        .report_logical_receiver(&resource, old, Outcome::Prepared(binding.clone()))
        .unwrap();
    assert!(client.next_frame().unwrap().is_none());

    let (current, _) = client.logical_offer(&resource).unwrap();
    assert_eq!(current.reference, old.reference);
    assert_eq!(current.source_revision, 3);
    client
        .report_logical_receiver(&resource, current, Outcome::Prepared(binding.clone()))
        .unwrap();
    let records = codec::decode_client(&client.next_frame().unwrap().unwrap())
        .unwrap()
        .records;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].owned, 1);
    assert!(records[0].desired.is_none());
    assert!(matches!(
        records[0].execution.as_ref().and_then(|report| report.outcome.as_ref()),
        Some(wire::execution_report::Outcome::LogicalReceiver(report))
            if report.source_revision == 3 && report.outcome == Some(Outcome::Prepared(binding.clone()))
    ));
    client
        .report_logical_receiver(&resource, current, Outcome::Prepared(binding))
        .unwrap();
    assert!(client.next_frame().unwrap().is_none());

    client.stage(resource, None).unwrap();
    assert_eq!(next(&mut client)[0].1.revision, 3);
    client
        .apply(&frame(vec![record(1, absent(3))], false))
        .unwrap();
    assert!(client.source("new").is_none());
    assert_eq!(client.retained_bytes, baseline);
}

#[test]
fn receiver_failure_has_its_own_revision_and_cannot_retarget_or_revive_a_lifetime() {
    let mut client = NativeState::new(1024 * 1024, participant()).unwrap();
    let resource = Resource::Reception(MediaSlot::RemoteAudio(0));
    let format = wire::CodecFormat::from(wire::codec_format::Codec::Opus(
        wire::OpusParameters::default(),
    ));
    let parameters = audio_parameters("receiver-proof".into());
    let mut desired = wire::ResourceSpec {
        declaration: Some(wire::resource_spec::Declaration::Subscription(
            wire::SubscriptionSpec {
                publication_id: "source".into(),
                kind: wire::TrackKind::Audio as i32,
                source_room: "room".into(),
                binding: Some(wire::TrackBinding {
                    transport: Some(wire::track_binding::Transport::Rtp(parameters.clone())),
                }),
            },
        )),
        settings: Some(wire::ResourceSettings {
            enabled: Some(true),
            receive: Some(wire::resource_settings::Receive::Audio(Default::default())),
        }),
    };
    let plan = wire::TrackBinding {
        transport: Some(wire::track_binding::Transport::Rtp(parameters)),
    };
    client
        .stage(resource.clone(), Some(desired.clone()))
        .unwrap();
    assert_eq!(next(&mut client)[0].1.revision, 1);
    let accepted = wire::ConfirmedState {
        evaluated_revision: 1,
        rejection: None,
        value: Some(wire::confirmed_state::Value::Owned(Box::new(
            wire::LiveResource {
                data_v1_source: None,
                spec: Some(desired.clone()),
                participant: Some(wire::Participant {
                    participant_id: "peer".into(),

                    room_id: "room".into(),
                }),
                track: Some(wire::Track {
                    publication_id: "source".into(),
                    participant_id: "peer".into(),
                    room_id: "room".into(),
                    track_id: "microphone".into(),
                    kind: Some(wire::track::Kind::Audio(wire::AudioTrack {
                        format: Some(format),
                    })),
                }),
                execution: wire::ExecutionStatus::Installed as i32,
                execution_failure: None,
                activity: None,
            },
        ))),
    };
    client
        .apply(&frame(vec![record(1, accepted.clone())], false))
        .unwrap();
    assert!(
        client.next_frame().unwrap().is_none(),
        "admission needs no installation report"
    );
    desired.settings.as_mut().unwrap().enabled = Some(false);
    client.stage(resource.clone(), Some(desired)).unwrap();
    assert_eq!(next(&mut client)[0].1.revision, 2);
    assert!(client.owned[&1].awaiting_evaluation());

    let outcome = wire::Failure {
        code: wire::FailureCode::InternalFailure as i32,
        diagnostic: "receiver failed after admission".into(),
    };
    let budget = client.memory_limit;
    client.memory_limit = client.retained_bytes;
    assert!(client.report_execution(1, outcome.clone()).is_err());
    assert!(client.executions.is_empty());
    client.memory_limit = budget;
    client.report_execution(1, outcome).unwrap();
    let records = codec::decode_client(&client.next_frame().unwrap().unwrap())
        .unwrap()
        .records;
    assert_eq!(records.len(), 1);
    assert!(records[0].desired.is_none());
    assert_eq!(records[0].execution.as_ref().unwrap().revision, 3);
    assert_eq!(client.owned[&1].sent_revision, 2);
    assert!(client.owned[&1].awaiting_evaluation());
    assert!(client.next_frame().unwrap().is_none());

    let mut changed = accepted;
    let Some(wire::confirmed_state::Value::Owned(live)) = changed.value.as_mut() else {
        panic!("owned receive plan fixture")
    };
    let Some(wire::resource_spec::Declaration::Subscription(subscription)) =
        live.spec.as_mut().unwrap().declaration.as_mut()
    else {
        panic!("receiver fixture")
    };
    let Some(wire::track_binding::Transport::Rtp(binding)) =
        subscription.binding.as_mut().unwrap().transport.as_mut()
    else {
        panic!("RTP receiver fixture")
    };
    binding.encodings[0].ssrc = Some(88);
    assert!(
        client
            .apply(&frame(vec![record(1, changed)], false))
            .is_err()
    );

    client
        .report_execution(
            1,
            wire::Failure {
                code: wire::FailureCode::InternalFailure as i32,
                diagnostic: "platform rejected receiver".into(),
            },
        )
        .unwrap();
    client.stage(resource, None).unwrap();
    let records = codec::decode_client(&client.next_frame().unwrap().unwrap())
        .unwrap()
        .records;
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].execution.as_ref().unwrap().revision, 4);
    assert!(matches!(
        records[0].execution.as_ref().unwrap().outcome,
        Some(wire::execution_report::Outcome::Failure(_))
    ));
    assert_eq!(records[1].desired.as_ref().unwrap().revision, 5);
    assert!(matches!(
        records[1].desired.as_ref().unwrap().value,
        Some(wire::desired_state::Value::Absent(true))
    ));
    client
        .report_execution(
            1,
            wire::Failure {
                code: wire::FailureCode::InternalFailure as i32,
                diagnostic: "late retired receiver failure".into(),
            },
        )
        .unwrap();
    assert!(client.next_frame().unwrap().is_none());
    let changes = client
        .apply(&frame(vec![record(1, absent(5))], false))
        .unwrap();
    assert!(
        matches!(&changes[0], Change::Owned { retired_binding: Some(binding), .. } if binding.as_ref() == &plan)
    );
    assert!(client.executions.is_empty());
    assert_eq!(client.retained_bytes, 0);
}
