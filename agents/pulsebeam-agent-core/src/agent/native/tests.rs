use super::*;
use alloc::boxed::Box;
use state::codec;

use crate::tests::{
    RunReconciliation,
    native_connection::{confirmed, connected, grant, observe},
};

#[test]
fn media_allocation_fences_callbacks_and_retires_superseded_physical_facts() {
    use crate::client_sdp::{Allocation, ApplyOutcome, Bindings};
    for slot in [
        MediaSlot::LocalVideo("caller-coordinate".into()),
        MediaSlot::LocalAudio("caller-coordinate".into()),
        MediaSlot::RemoteVideo(256),
        MediaSlot::RemoteAudio(256),
    ] {
        let (mut agent, generation, _) = connected(crate::TopicRegistrations::default());
        let format = state::CodecFormat::from(match slot.kind() {
            crate::MediaKind::Video => state::codec_format::Codec::Vp8(Default::default()),
            crate::MediaKind::Audio => state::codec_format::Codec::Opus(Default::default()),
        });
        let receiving = slot.direction() == crate::MediaDirection::ReceiveOnly;
        let requested_format = receiving.then_some(format.clone());
        let want = |track_id: &str| MediaWant {
            resource: if receiving {
                Resource::Reception(slot.clone())
            } else {
                Resource::Publication {
                    kind: if slot.kind() == crate::MediaKind::Video {
                        state::TrackKind::Video
                    } else {
                        state::TrackKind::Audio
                    },
                    track_id: track_id.into(),
                }
            },
            source: receiving.then(|| track_id.into()),
            offer: None,
            allocation: Allocation {
                slot: slot.clone(),
                format: requested_format.clone(),
            },
        };
        let assigned = SlotBinding {
            slot: slot.clone(),
            mid: "actual".into(),
            media_index: 1,
            kind: slot.kind(),
            direction: slot.direction(),
        };
        let (mut binding, capabilities) = crate::rtc_preparation::binding_fixture(format);
        binding.key = crate::rtc_preparation::bootstrap_media_key(&slot);
        binding.mid = assigned.mid.clone();
        let bindings = if receiving {
            Bindings {
                publications: Vec::new(),
                receptions: vec![binding],
            }
        } else {
            Bindings {
                publications: vec![binding],
                receptions: Vec::new(),
            }
        };
        assert!(
            crate::rtc_preparation::prepare_bootstrap_bindings(
                bindings.clone(),
                &[assigned.clone()],
                &capabilities
            )
            .unwrap()[&slot]
                .is_ok()
        );
        for preparation in [
            None,
            Some(Ok(BTreeMap::new())),
            Some(Ok(BTreeMap::from([(
                slot.clone(),
                Err(crate::client_sdp::Error::Observation),
            )]))),
        ] {
            let resource = want("camera").resource;
            let active = agent.active.as_mut().unwrap();
            active
                .coordinates
                .insert(slot.clone(), Some(assigned.clone()));
            active.media_preparation = preparation;
            assert!(rtp(active, &mut agent.logs, &resource, &slot, None).is_none());
            assert!(active.media_leases[&slot].retired);
            assert_eq!(active.media_leases[&slot].mid, assigned.mid);
            retire_media(active, &mut agent.ids, &mut agent.effects);
            let Some(Effect::Rtc(RtcEffect::RetireMedia {
                operation, mids, ..
            })) = agent.next_effect_after_turn()
            else {
                panic!("incomplete bootstrap retirement");
            };
            assert_eq!(mids, [assigned.mid.clone()]);
            agent
                .handle(HostEvent::Rtc(RtcEvent::MediaPrepared {
                    generation,
                    operation,
                    slots: Vec::new(),
                    outcome: ApplyOutcome::Applied,
                    preparation: Ok(Bindings::default()),
                }))
                .unwrap();
            let active = agent.active.as_mut().unwrap();
            assert!(active.coordinates.contains_key(&slot));
            assert!(active.coordinates[&slot].is_none());
            assert!(!active.media_leases.contains_key(&slot));
            assert!(rtp(active, &mut agent.logs, &resource, &slot, None).is_none());
            assert!(!active.media_leases.contains_key(&slot));
            assert!(agent.next_effect_after_turn().is_none());
        }
        let active = agent.active.as_mut().unwrap();
        active.capabilities.rtp = capabilities;
        active.media_wanted.insert(slot.clone(), want("camera"));
        allocate_media(active, &mut agent.ids, &mut agent.effects);
        let Some(Effect::Rtc(RtcEffect::AllocateMedia {
            operation,
            allocations,
            ..
        })) = agent.next_effect_after_turn()
        else {
            panic!("allocation")
        };
        assert_eq!(
            agent.source_preparation(&slot).readiness,
            SourceReadiness::Preparing
        );
        assert_eq!(
            agent
                .source_preparation(&MediaSlot::LocalAudio("a0".into()))
                .readiness,
            SourceReadiness::Ready
        );
        assert_eq!(
            allocations,
            [Allocation {
                slot: slot.clone(),
                format: requested_format.clone()
            }]
        );
        let stale_generation = agent.ids.generation();
        for (callback_generation, callback_operation) in [
            (stale_generation, operation),
            (generation, agent.ids.operation()),
        ] {
            agent
                .handle(HostEvent::Rtc(RtcEvent::MediaPrepared {
                    generation: callback_generation,
                    operation: callback_operation,
                    slots: vec![assigned.clone()],
                    outcome: ApplyOutcome::Applied,
                    preparation: Ok(bindings.clone()),
                }))
                .unwrap();
            assert!(agent.active.as_ref().unwrap().media_allocation.is_some());
            assert_eq!(
                agent.source_preparation(&slot).readiness,
                SourceReadiness::Preparing
            );
            assert!(agent.next_effect_after_turn().is_none());
        }
        agent
            .active
            .as_mut()
            .unwrap()
            .media_wanted
            .insert(slot.clone(), want("screen"));
        agent
            .handle(HostEvent::Rtc(RtcEvent::MediaPrepared {
                generation,
                operation,
                slots: vec![assigned],
                outcome: ApplyOutcome::Applied,
                preparation: Ok(bindings),
            }))
            .unwrap();
        let active = agent.active.as_ref().unwrap();
        assert!(active.media_allocation.is_none());
        assert!(
            active
                .media_preparation
                .as_ref()
                .is_none_or(|cache| cache.as_ref().is_ok_and(|cache| !cache.contains_key(&slot)))
        );
        let lease = &active.media_leases[&slot];
        assert!(lease.retired);
        assert_eq!(lease.resource, want("camera").resource);
        let Some(Effect::Rtc(RtcEffect::RetireMedia {
            operation, mids, ..
        })) = agent.next_effect_after_turn()
        else {
            panic!("orphan retirement")
        };
        assert_eq!(mids, ["actual"]);
        assert!(agent.next_effect_after_turn().is_none());
        agent
            .handle(HostEvent::Rtc(RtcEvent::MediaPrepared {
                generation,
                operation,
                slots: Vec::new(),
                outcome: ApplyOutcome::Applied,
                preparation: Ok(Bindings::default()),
            }))
            .unwrap();
        assert!(
            !agent
                .active
                .as_ref()
                .unwrap()
                .media_leases
                .contains_key(&slot)
        );
        assert!(agent.next_effect_after_turn().is_none());
    }

    let (mut agent, generation, channel) = connected(crate::TopicRegistrations::default());
    observe(
        &mut agent,
        generation,
        channel,
        ["a-source", "z-source"]
            .into_iter()
            .map(|id| {
                confirmed(
                    state::resource_key::Key::Publication(id.into()),
                    0,
                    state::confirmed_state::Value::Track(state::Track {
                        publication_id: id.into(),
                        participant_id: "remote".into(),
                        room_id: "room".into(),
                        track_id: "voice".into(),
                        kind: Some(state::track::Kind::Audio(state::AudioTrack {
                            format: Some(state::CodecFormat::from(
                                state::codec_format::Codec::Opus(Default::default()),
                            )),
                        })),
                    }),
                )
            })
            .collect(),
    );
    let mut desired = DesiredState {
        revision: agent.declared.revision + 1,
        connected: true,
        audio: crate::AudioSubscription {
            pinned: vec!["z-source".into()],
            ..Default::default()
        },
        ..Default::default()
    };
    agent
        .command(AgentCommand::ReplaceDesired(desired.clone()))
        .unwrap();
    let Some(Effect::Rtc(RtcEffect::AllocateMedia {
        operation,
        allocations,
        ..
    })) = agent.next_effect_after_turn()
    else {
        panic!("audio allocation")
    };
    assert_eq!(allocations[0].slot, MediaSlot::RemoteAudio(0));
    desired.revision += 1;
    desired.audio.pinned.push("a-source".into());
    agent
        .command(AgentCommand::ReplaceDesired(desired))
        .unwrap();
    assert!(agent.next_effect_after_turn().is_none());
    let active = agent.active.as_ref().unwrap();
    assert_eq!(
        active.media_allocation.as_ref().unwrap().operation,
        operation
    );
    assert_eq!(
        active.media_wanted[&MediaSlot::RemoteAudio(0)]
            .source
            .as_deref(),
        Some("z-source")
    );
    assert_eq!(
        active.media_wanted[&MediaSlot::RemoteAudio(1)]
            .source
            .as_deref(),
        Some("a-source")
    );
    assert!(agent.next_effect_after_turn().is_none());
}

#[test]
fn media_retirement_is_correlated_and_cannot_clear_replacement_facts() {
    use crate::client_sdp::{ApplyOutcome, Bindings, Error};
    let (mut agent, generation, _) = connected(crate::TopicRegistrations::default());
    let slot = MediaSlot::LocalVideo("v0".into());
    let resource = Resource::Publication {
        kind: state::TrackKind::Video,
        track_id: "camera".into(),
    };
    let cached = |mid: &str| crate::rtc_preparation::PreparedRtp {
        binding: state::TrackBinding {
            transport: Some(state::track_binding::Transport::Rtp(state::RtpParameters {
                extmap_allow_mixed: None,
                mid: mid.into(),
                ..Default::default()
            })),
        },
        encodings: Vec::new(),
    };
    let seed = |agent: &mut Agent, mid: &str, retired| {
        let active = agent.active.as_mut().unwrap();
        active.media_preparation = Some(Ok([(slot.clone(), Ok(cached(mid)))].into()));
        active.coordinates.insert(
            slot.clone(),
            Some(SlotBinding {
                slot: slot.clone(),
                mid: mid.into(),
                media_index: 1,
                kind: crate::MediaKind::Video,
                direction: crate::MediaDirection::SendOnly,
            }),
        );
        active.media_leases.insert(
            slot.clone(),
            MediaLease {
                resource: resource.clone(),
                mid: mid.into(),
                identity_pending: false,
                retired,
                retirement_failed: false,
                wire_retired: false,
                source: None,
                offer: None,
            },
        );
    };
    let start = |agent: &mut Agent, mid: &str| {
        let active = agent.active.as_mut().unwrap();
        retire_media(active, &mut agent.ids, &mut agent.effects);
        let Some(Effect::Rtc(RtcEffect::RetireMedia {
            generation: actual,
            operation,
            mids,
        })) = agent.next_effect_after_turn()
        else {
            panic!("retirement")
        };
        assert_eq!(actual, generation);
        assert_eq!(mids, [mid]);
        assert!(agent.next_effect_after_turn().is_none());
        operation
    };
    seed(&mut agent, "old", false);
    assert_eq!(
        agent.source_preparation(&slot).readiness,
        SourceReadiness::Ready
    );
    agent
        .active
        .as_mut()
        .unwrap()
        .media_leases
        .get_mut(&slot)
        .unwrap()
        .identity_pending = true;
    assert_eq!(
        agent.source_preparation(&slot).readiness,
        SourceReadiness::Ready,
        "input may establish pending sender identity"
    );
    retire_media(
        agent.active.as_mut().unwrap(),
        &mut agent.ids,
        &mut agent.effects,
    );
    assert!(
        agent.next_effect_after_turn().is_none(),
        "live leases cannot retire"
    );
    agent
        .active
        .as_mut()
        .unwrap()
        .media_leases
        .get_mut(&slot)
        .unwrap()
        .retired = true;
    let operation = start(&mut agent, "old");
    assert_eq!(
        agent.source_preparation(&slot).readiness,
        SourceReadiness::Preparing
    );
    assert_eq!(
        agent
            .source_preparation(&MediaSlot::LocalAudio("a0".into()))
            .readiness,
        SourceReadiness::Ready
    );
    let wrong_operation = agent.ids.operation();
    let wrong_generation = agent.ids.generation();
    for (generation, operation) in [(generation, wrong_operation), (wrong_generation, operation)] {
        agent
            .handle(HostEvent::Rtc(RtcEvent::MediaPrepared {
                generation,
                operation,
                slots: Vec::new(),
                outcome: ApplyOutcome::Applied,
                preparation: Ok(Bindings::default()),
            }))
            .unwrap();
        assert!(agent.active.as_ref().unwrap().media_retirement.is_some());
        assert_eq!(
            agent.source_preparation(&slot).readiness,
            SourceReadiness::Preparing
        );
        assert_eq!(
            agent.active.as_ref().unwrap().media_leases[&slot].mid,
            "old"
        );
        assert!(agent.next_effect_after_turn().is_none());
    }
    agent
        .handle(HostEvent::Rtc(RtcEvent::MediaPrepared {
            generation,
            operation,
            slots: Vec::new(),
            outcome: ApplyOutcome::Applied,
            preparation: Ok(Bindings::default()),
        }))
        .unwrap();
    let active = agent.active.as_ref().unwrap();
    assert!(!active.media_leases.contains_key(&slot));
    assert!(active.coordinates[&slot].is_none());
    assert!(
        !active
            .media_preparation
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .contains_key(&slot)
    );
    seed(&mut agent, "another-old", true);
    let operation = start(&mut agent, "another-old");
    seed(&mut agent, "replacement", false);
    agent
        .handle(HostEvent::Rtc(RtcEvent::MediaPrepared {
            generation,
            operation,
            slots: Vec::new(),
            outcome: ApplyOutcome::Applied,
            preparation: Ok(Bindings::default()),
        }))
        .unwrap();
    assert_eq!(
        agent.source_preparation(&slot).readiness,
        SourceReadiness::Ready
    );
    let active = agent.active.as_ref().unwrap();
    assert_eq!(active.media_leases[&slot].mid, "replacement");
    assert_eq!(
        active.coordinates[&slot].as_ref().unwrap().mid,
        "replacement"
    );
    assert_eq!(
        active.media_preparation.as_ref().unwrap().as_ref().unwrap()[&slot]
            .as_ref()
            .unwrap(),
        &cached("replacement")
    );
    assert!(agent.next_effect_after_turn().is_none());
    agent
        .active
        .as_mut()
        .unwrap()
        .media_leases
        .get_mut(&slot)
        .unwrap()
        .retired = true;
    let operation = start(&mut agent, "replacement");
    agent
        .handle(HostEvent::Rtc(RtcEvent::MediaPrepared {
            generation,
            operation,
            slots: Vec::new(),
            outcome: ApplyOutcome::NotApplied,
            preparation: Err(Error::Observation),
        }))
        .unwrap();
    assert!(agent.active.as_ref().unwrap().media_leases[&slot].retirement_failed);
    assert_eq!(
        agent.source_preparation(&slot).readiness,
        SourceReadiness::Failed
    );
    assert!(
        agent.next_effect_after_turn().is_none(),
        "failed retirement cannot spin or reuse old facts"
    );
    seed(&mut agent, "fenced", true);
    let operation = start(&mut agent, "fenced");
    agent.fence_signaling(generation);
    assert_eq!(
        agent.next_effect_after_turn(),
        Some(Effect::Rtc(RtcEffect::CancelMediaPreparation {
            generation,
            operation
        }))
    );
    agent
        .handle(HostEvent::Rtc(RtcEvent::MediaPrepared {
            generation,
            operation,
            slots: Vec::new(),
            outcome: ApplyOutcome::Applied,
            preparation: Ok(Bindings::default()),
        }))
        .unwrap();
    assert_eq!(
        agent.active.as_ref().unwrap().media_leases[&slot].mid,
        "fenced"
    );
    assert!(agent.next_effect_after_turn().is_none());
}

#[test]
fn quic_reserves_nonreused_payload_ids_without_rtc_effects() {
    let (mut agent, _, _) = connected(crate::TopicRegistrations::default());
    let session = agent.active.as_mut().unwrap();
    session.state = crate::native_state::NativeState::with_profile(
        1 << 20,
        None,
        state::validation::BindingProfile::Quic,
    )
    .unwrap();
    session.data_preparations.clear();
    let resource = Resource::Publication {
        kind: state::TrackKind::Data,
        track_id: "events".into(),
    };
    let mut ids = IdGenerator::new();
    let mut effects = VecDeque::new();
    let reserve = |session: &mut Session, ids: &mut IdGenerator, effects: &mut VecDeque<Effect>| {
        payload(
            session,
            &resource,
            crate::TopicChannel::Publisher(crate::TopicPublisher {
                topic: "events".into(),
                mode: crate::TopicMode::Ordered,
            }),
            None,
            state::DataV1Delivery::Reliable,
            ids,
            effects,
        )
        .and_then(|binding| state::DataV1Binding::from_track(&binding))
    };
    let Some(state::DataV1Binding::Quic(first)) = reserve(session, &mut ids, &mut effects) else {
        panic!("QUIC reservation")
    };
    assert_eq!(first.binding_id, 1);
    assert_eq!(
        reserve(session, &mut ids, &mut effects),
        Some(state::DataV1Binding::Quic(first))
    );
    let previous = session.payload_resources[&resource];
    assert!(retire_quic_payload(
        session,
        &resource,
        previous,
        &mut effects
    ));
    assert_eq!(
        effects.pop_front(),
        Some(Effect::Quic(crate::QuicEffect::RetireData {
            generation: session.generation,
            channel: previous,
        }))
    );
    session.reusable_streams.insert(1);
    let Some(state::DataV1Binding::Quic(second)) = reserve(session, &mut ids, &mut effects) else {
        panic!("replacement QUIC reservation")
    };
    assert_eq!(second.binding_id, 2);
    assert!(second.incarnation > first.incarnation);
    let audio = Resource::Publication {
        kind: state::TrackKind::Audio,
        track_id: "voice".into(),
    };
    session.capabilities.rtp = state::RtpCapabilities::default();
    assert!(quic_rtp_binding(session, &audio, None, &mut ids).is_none());
    session
        .capabilities
        .rtp
        .codecs
        .push(state::RtpCodecCapability {
            format: Some(state::CodecFormat::from(state::codec_format::Codec::Opus(
                state::OpusParameters::default(),
            ))),
            ..Default::default()
        });
    let binding = quic_rtp_binding(session, &audio, None, &mut ids).unwrap();
    assert!(
        matches!(&binding.transport, Some(state::track_binding::Transport::QuicRtp(parameters)) if parameters.binding_id == 3 && parameters.primary_ssrc != 0)
    );
    let spec = publication(
        "voice",
        true,
        state::publication_spec::Kind::Audio(state::AudioPublication {
            binding: Some(binding.clone()),
        }),
    );
    session.state.stage(audio.clone(), Some(spec)).unwrap();
    assert_eq!(
        quic_rtp_binding(session, &audio, None, &mut ids),
        Some(binding)
    );
    let receiver = Resource::Reception(MediaSlot::RemoteAudio(0));
    let binding = quic_rtp_binding(session, &receiver, Some("publication"), &mut ids).unwrap();
    assert!(
        matches!(&binding.transport, Some(state::track_binding::Transport::QuicRtp(parameters)) if parameters.binding_id == 4)
    );
    let spec = subscription(
        "publication".into(),
        "stage".into(),
        state::TrackKind::Audio,
        binding.clone(),
        audio_options(),
    );
    session.state.stage(receiver.clone(), Some(spec)).unwrap();
    assert_eq!(
        quic_rtp_binding(session, &receiver, Some("publication"), &mut ids),
        Some(binding)
    );
    assert!(quic_rtp_binding(session, &receiver, Some("another"), &mut ids).is_none());
    assert!(effects.is_empty());
}

#[test]
fn active_control_declares_only_prepared_payloads_and_retries_the_exact_serialized_frame() {
    let publisher = crate::TopicPublisher {
        topic: "Literal_name".into(),
        mode: crate::TopicMode::Ordered,
    };
    let (mut agent, generation, control) = connected(crate::TopicRegistrations {
        publishers: vec![publisher.clone()],
        subscribers: Vec::new(),
    });
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Prepared {
            generation,
            channel: control,
            preparation: Ok(crate::PreparedDataChannel {
                stream_id: 0,
                negotiated: true,
                ordered: true,
                reliability: DataChannelReliability::Reliable,
                association_max_channels: Some(2),
            }),
        }))
        .unwrap();
    let Some(Effect::Rtc(RtcEffect::CreateDataChannel {
        generation: actual,
        channel,
        specification,
    })) = agent.next_effect_after_turn()
    else {
        panic!("payload allocation")
    };
    assert_eq!(actual, generation);
    assert_eq!(specification.negotiated_id, Some(1));
    assert!(!specification.ordered);
    assert!(agent.next_effect_after_turn().is_none());
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Prepared {
            generation,
            channel,
            preparation: Ok(crate::PreparedDataChannel {
                stream_id: 1,
                negotiated: true,
                ordered: true,
                reliability: DataChannelReliability::Reliable,
                association_max_channels: Some(2),
            }),
        }))
        .unwrap();
    assert!(
        agent.next_effect_after_turn().is_none(),
        "requested properties cannot override incompatible host facts"
    );
    assert!(
        agent.snapshot().topics.publishers[0]
            .publication_id
            .is_none()
    );
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Closed {
            generation,
            channel,
            reset_complete: true,
        }))
        .unwrap();
    let Some(Effect::Rtc(RtcEffect::CreateDataChannel {
        generation: actual,
        channel: replacement,
        specification,
    })) = agent.next_effect_after_turn()
    else {
        panic!("fresh payload after actual closure")
    };
    assert_eq!(actual, generation);
    assert_ne!(replacement, channel);
    assert_eq!(specification.negotiated_id, Some(1));
    assert!(!specification.ordered);
    assert_eq!(
        specification.reliability,
        DataChannelReliability::MaxRetransmits(0)
    );
    assert_eq!(agent.snapshot().generation, Some(generation));
    assert_eq!(
        agent.snapshot().connection,
        crate::ConnectionState::Connected
    );
    let before = agent.snapshot().clone();
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Prepared {
            generation,
            channel,
            preparation: Ok(crate::PreparedDataChannel {
                stream_id: 1,
                negotiated: true,
                ordered: false,
                reliability: DataChannelReliability::MaxRetransmits(0),
                association_max_channels: Some(2),
            }),
        }))
        .unwrap();
    assert_eq!(agent.snapshot(), &before);
    assert!(
        agent.next_effect_after_turn().is_none(),
        "late valid-looking facts cannot revive the closed physical identity"
    );
    let channel = replacement;
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Prepared {
            generation,
            channel,
            preparation: Ok(crate::PreparedDataChannel {
                stream_id: 1,
                negotiated: true,
                ordered: false,
                reliability: DataChannelReliability::MaxRetransmits(0),
                association_max_channels: Some(2),
            }),
        }))
        .unwrap();
    let Some(Effect::DataChannel(DataChannelEffect::Send {
        operation,
        payload,
        channel: sent_channel,
        ..
    })) = agent.next_effect_after_turn()
    else {
        panic!("StateBatch")
    };
    assert_eq!(sent_channel, control);
    let batch = codec::decode_client(&payload).unwrap();
    let record = &batch.records[0];
    let reference = &record.owned;
    let Some(desired) = &record.desired else {
        panic!("desired")
    };
    let Some(state::desired_state::Value::Present(spec)) = &desired.value else {
        panic!("present")
    };
    let Some(state::resource_spec::Declaration::Publication(declaration)) = &spec.declaration
    else {
        panic!("publication")
    };
    assert_eq!(declaration.track_id, publisher.topic);
    let Some(state::publication_spec::Kind::Data(data)) = &declaration.kind else {
        panic!("DATA publication")
    };
    assert!(matches!(
        data.binding
            .as_ref()
            .and_then(|binding| binding.transport.as_ref()),
        Some(state::track_binding::Transport::SctpDataV1(_))
    ));
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::SendFailed {
            generation,
            channel: control,
            operation,
            message: "not admitted".into(),
        }))
        .unwrap();
    let Some(Effect::Timer(TimerEffect::Schedule { timer, .. })) = agent.next_effect_after_turn()
    else {
        panic!("retry timer")
    };
    let mut successor = agent.desired.clone();
    successor.revision += 1;
    successor.topics.publishers.clear();
    agent
        .command(AgentCommand::ReplaceDesired(successor))
        .unwrap();
    assert!(agent.next_effect_after_turn().is_none());
    agent
        .handle(HostEvent::Timer(TimerEvent::Fired { timer }))
        .unwrap();
    let Some(Effect::DataChannel(DataChannelEffect::Send {
        operation: retry,
        payload: retried,
        ..
    })) = agent.next_effect_after_turn()
    else {
        panic!("retry")
    };
    assert_ne!(operation, retry);
    assert_eq!(payload, retried);
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Sent {
            generation,
            channel: control,
            operation: retry,
        }))
        .unwrap();
    let Some(Effect::DataChannel(DataChannelEffect::Send {
        payload: retirement,
        ..
    })) = agent.next_effect_after_turn()
    else {
        panic!("serialized retirement")
    };
    let retired = codec::decode_client(&retirement).unwrap();
    assert_eq!(retired.records[0].owned, *reference);
    assert!(
        matches!(&retired.records[0].desired, Some(value) if matches!(value.value, Some(state::desired_state::Value::Absent(true))))
    );
    observe(
        &mut agent,
        generation,
        control,
        vec![confirmed(
            state::resource_key::Key::Owned(*reference),
            desired.revision,
            state::confirmed_state::Value::Owned(Box::new(state::LiveResource {
                data_v1_source: Some(3),
                spec: Some(spec.as_ref().clone()),
                participant: Some(state::Participant {
                    participant_id: "p1".into(),

                    room_id: "room".into(),
                }),
                track: Some(state::Track {
                    publication_id: "server-canonical-source".into(),
                    participant_id: "p1".into(),
                    room_id: "room".into(),
                    track_id: publisher.topic.clone(),
                    kind: Some(state::track::Kind::Data(state::DataTrack {
                        capture_clock: None,
                        delivery: state::DataDelivery::Ordered as i32,
                    })),
                }),
                execution: state::ExecutionStatus::Installed as i32,
                execution_failure: None,
                activity: None,
            })),
        )],
    );
    assert!(agent.desired.topics.publishers.is_empty());
    assert!(agent.next_data_v1_effect().is_none());
    assert!(agent.snapshot.terminal_failure.is_none());
    assert!(agent.next_effect_after_turn().is_none());
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Closed {
            generation,
            channel,
            reset_complete: true,
        }))
        .unwrap();
    let mut successor = agent.desired.clone();
    successor.revision += 1;
    successor.topics.publishers = vec![publisher];
    agent
        .command(AgentCommand::ReplaceDesired(successor))
        .unwrap();
    assert!(
        agent.next_effect_after_turn().is_none(),
        "reset completion cannot release a stream retained by the server"
    );
    let Some(absent) = &retired.records[0].desired else {
        panic!("retirement desired state")
    };
    observe(
        &mut agent,
        generation,
        control,
        vec![confirmed(
            state::resource_key::Key::Owned(*reference),
            absent.revision,
            state::confirmed_state::Value::Absent(state::AbsentState {
                reason: state::AbsenceReason::Removed as i32,
            }),
        )],
    );
    let Some(Effect::Rtc(RtcEffect::CreateDataChannel {
        generation: actual,
        channel: fresh,
        specification,
    })) = agent.next_effect_after_turn()
    else {
        panic!("reuse after both reset and authoritative cutoff")
    };
    assert_eq!(actual, generation);
    assert_ne!(fresh, channel);
    assert_eq!(specification.negotiated_id, Some(1));
    let before = agent.snapshot().clone();
    for event in [
        DataChannelEvent::Opened {
            generation,
            channel,
        },
        DataChannelEvent::Message {
            generation,
            channel,
            binary: true,
            payload: vec![0xff],
        },
        DataChannelEvent::Sent {
            operation,
            generation,
            channel,
        },
        DataChannelEvent::Closed {
            generation,
            channel,
            reset_complete: true,
        },
    ] {
        agent.handle(HostEvent::DataChannel(event)).unwrap();
        assert_eq!(agent.snapshot(), &before);
        assert!(agent.next_effect_after_turn().is_none());
    }
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Closed {
            generation,
            channel: fresh,
            reset_complete: false,
        }))
        .unwrap();
    assert!(
        agent.next_effect_after_turn().is_none(),
        "generic closure without a reset barrier quarantines the only payload SID"
    );
    assert_eq!(agent.snapshot().generation, Some(generation));
    assert_eq!(
        agent.snapshot().connection,
        crate::ConnectionState::Connected
    );
}

#[test]
fn renewal_retries_original_precondition_and_commits_credentials_only_after_ordered_grant() {
    use crate::CredentialStatus;

    let patch = |agent: &mut Agent| {
        let Some(Effect::Http(HttpEffect::Request {
            operation, request, ..
        })) = agent.next_effect_after_turn()
        else {
            panic!("conditional PATCH")
        };
        assert_eq!(request.method, HttpMethod::Patch);
        (operation, request)
    };
    let retry = |agent: &mut Agent| {
        let Some(Effect::Timer(TimerEffect::Schedule { timer, after })) =
            agent.next_effect_after_turn()
        else {
            panic!("bounded authorization retry")
        };
        assert!(!after.is_zero());
        agent
            .handle(HostEvent::Timer(TimerEvent::Fired { timer }))
            .unwrap();
        patch(agent)
    };
    let (mut agent, generation, control) = connected(crate::TopicRegistrations::default());
    agent
        .command(AgentCommand::RenewAuthorization("replacement".into()))
        .unwrap();
    let (operation, original) = patch(&mut agent);
    assert!(
        original
            .headers
            .iter()
            .any(|header| header.name == "If-Match" && header.value == "\"auth-1\"")
    );
    for token in ["superseded", "newest"] {
        agent
            .command(AgentCommand::RenewAuthorization(token.into()))
            .unwrap();
    }
    assert_eq!(agent.snapshot().credential_revision, 3);
    assert!(
        agent.next_effect_after_turn().is_none(),
        "new desire cannot overlap the issued PATCH"
    );
    agent
        .handle(HostEvent::Http(HttpEvent::Failed {
            operation,
            message: "private failure must not enter state".into(),
        }))
        .unwrap();
    observe(&mut agent, generation, control, vec![grant(2)]);
    assert_eq!(agent.config.token, "newest");
    assert_eq!(agent.snapshot().accepted_credential_revision, Some(0));
    assert_eq!(
        agent
            .snapshot()
            .credential_outcome
            .as_ref()
            .unwrap()
            .revision,
        1
    );
    assert_eq!(
        agent.snapshot().credential_outcome.as_ref().unwrap().status,
        CredentialStatus::Uncertain
    );
    assert!(!format!("{:?}", agent.snapshot()).contains("private failure"));
    let (retry_operation, retried) = retry(&mut agent);
    assert_eq!(
        retried, original,
        "transport ambiguity retains the original credential and precondition"
    );
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation: retry_operation,
            response: HttpResponse {
                status: 412,
                headers: vec![HttpHeader {
                    name: "ETag".into(),
                    value: "\"auth-4\"".into(),
                }],
                body: vec![],
            },
        }))
        .unwrap();
    assert_eq!(
        agent
            .snapshot()
            .credential_outcome
            .as_ref()
            .unwrap()
            .revision,
        1,
        "old uncertainty is not attributed to unissued newest desire"
    );
    let before = agent.snapshot().clone();
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation,
            response: HttpResponse {
                status: 204,
                headers: vec![HttpHeader {
                    name: "ETag".into(),
                    value: "\"auth-2\"".into(),
                }],
                body: vec![],
            },
        }))
        .unwrap();
    assert_eq!(
        agent.snapshot(),
        &before,
        "a superseded HTTP completion is fenced"
    );
    let (fresh_operation, fresh) = retry(&mut agent);
    assert!(
        fresh
            .headers
            .iter()
            .any(|header| header.name == "If-Match" && header.value == "\"auth-4\"")
    );
    assert!(
        fresh
            .headers
            .iter()
            .any(|header| header.name == "Authorization" && header.value == "Bearer newest")
    );
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation: fresh_operation,
            response: HttpResponse {
                status: 204,
                headers: vec![HttpHeader {
                    name: "ETag".into(),
                    value: "\"auth-5\"".into(),
                }],
                body: vec![],
            },
        }))
        .unwrap();
    assert_eq!(agent.snapshot().accepted_credential_revision, Some(0));
    agent
        .command(AgentCommand::RenewAuthorization("newer".into()))
        .unwrap();
    assert!(
        agent.next_effect_after_turn().is_none(),
        "HTTP acceptance still waits for its ordered grant"
    );
    observe(&mut agent, generation, control, vec![grant(5)]);
    assert_eq!(agent.config.token, "newer");
    assert_eq!(agent.snapshot().credential_revision, 4);
    assert_eq!(agent.snapshot().accepted_credential_revision, Some(3));
    assert_eq!(
        agent
            .snapshot()
            .credential_outcome
            .as_ref()
            .unwrap()
            .revision,
        3
    );
    assert_eq!(
        agent.snapshot().credential_outcome.as_ref().unwrap().status,
        CredentialStatus::Accepted
    );
    let (denied, newer) = patch(&mut agent);
    assert!(
        newer
            .headers
            .iter()
            .any(|header| header.name == "If-Match" && header.value == "\"auth-5\"")
    );
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation: denied,
            response: HttpResponse {
                status: 403,
                headers: vec![],
                body: vec![],
            },
        }))
        .unwrap();
    assert_eq!(
        agent
            .snapshot()
            .credential_outcome
            .as_ref()
            .unwrap()
            .revision,
        4
    );
    assert_eq!(
        agent.snapshot().credential_outcome.as_ref().unwrap().status,
        CredentialStatus::Rejected
    );
    assert_eq!(agent.snapshot().accepted_credential_revision, Some(3));
    assert!(
        agent.next_effect_after_turn().is_none(),
        "denied unchanged desire is not a retry loop"
    );

    agent.config.retry.maximum_attempts = 2;
    agent
        .command(AgentCommand::RenewAuthorization("newer".into()))
        .unwrap();
    assert_eq!(
        agent.snapshot().credential_revision,
        5,
        "explicit settled retry has a fresh revision"
    );
    let (uncertain, _) = patch(&mut agent);
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation: uncertain,
            response: HttpResponse {
                status: 204,
                headers: vec![],
                body: vec![],
            },
        }))
        .unwrap();
    let (uncertain_retry, _) = retry(&mut agent);
    agent
        .handle(HostEvent::Http(HttpEvent::Failed {
            operation: uncertain_retry,
            message: "lost".into(),
        }))
        .unwrap();
    let outcome = agent.snapshot().credential_outcome.as_ref().unwrap();
    assert_eq!(outcome.revision, 5);
    assert_eq!(outcome.status, CredentialStatus::Uncertain);
    assert_eq!(
        outcome.error.as_ref().unwrap().class,
        FailureClass::RetryExhausted
    );
    assert!(agent.next_effect_after_turn().is_none());
    agent
        .command(AgentCommand::RenewAuthorization("recover".into()))
        .unwrap();
    let (recovery, recovery_request) = patch(&mut agent);
    assert!(
        recovery_request
            .headers
            .iter()
            .any(|header| header.name == "Authorization" && header.value == "Bearer newer")
    );
    assert!(
        recovery_request
            .headers
            .iter()
            .any(|header| header.name == "If-Match" && header.value == "\"auth-5\"")
    );
    agent
        .handle(HostEvent::Http(HttpEvent::Failed {
            operation: recovery,
            message: "lost".into(),
        }))
        .unwrap();
    let Some(Effect::Timer(TimerEffect::Schedule { timer, after })) =
        agent.next_effect_after_turn()
    else {
        panic!("retry timer");
    };
    assert!(!after.is_zero());

    let mut disconnected = Agent::new(agent.config.clone()).unwrap();
    for agent in [&mut agent, &mut disconnected] {
        agent.snapshot.credential_revision = u64::MAX;
        let before = agent.snapshot().clone();
        let effects = agent.effects.clone();
        assert_eq!(
            agent.command(AgentCommand::RenewAuthorization("exhausted".into())),
            Err(AgentError::CredentialRevisionExhausted)
        );
        assert_eq!(agent.snapshot(), &before);
        assert_eq!(agent.effects, effects);
    }
    agent.command(AgentCommand::Disconnect).unwrap();
    let mut cancelled = false;
    let mut deleted = false;
    while let Some(effect) = agent.next_effect_after_turn() {
        match effect {
            Effect::Timer(TimerEffect::Cancel {
                timer: cancelled_timer,
            }) => {
                assert_eq!(cancelled_timer, timer);
                cancelled = true;
            }
            Effect::Rtc(RtcEffect::Close { generation: closed }) => assert_eq!(closed, generation),
            Effect::Http(HttpEffect::Request { request, .. }) => {
                assert_eq!(request.method, HttpMethod::Delete);
                assert!(request.headers.iter().any(
                    |header| header.name == "Authorization" && header.value == "Bearer newest"
                ));
                deleted = true;
            }
            effect => panic!("unexpected closing effect: {effect:?}"),
        }
    }
    assert!(cancelled && deleted);
    let before = agent.snapshot().clone();
    agent
        .handle(HostEvent::Timer(TimerEvent::Fired { timer }))
        .unwrap();
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation: recovery,
            response: HttpResponse {
                status: 204,
                headers: vec![HttpHeader {
                    name: "ETag".into(),
                    value: "\"auth-6\"".into(),
                }],
                body: vec![],
            },
        }))
        .unwrap();
    observe(&mut agent, generation, control, vec![grant(6)]);
    assert_eq!(agent.snapshot(), &before);
    assert!(agent.next_effect_after_turn().is_none());
}

#[test]
fn renewal_response_fences_preserve_issued_and_coalesce_unsent_credentials() {
    use crate::CredentialStatus;

    for (status, etags, body) in [
        (204, vec![], vec![]),
        (204, vec!["\"auth-2\""], vec![1]),
        (204, vec!["\"auth-2\"", "\"auth-3\""], vec![]),
        (204, vec!["\"auth-3\""], vec![]),
        (412, vec![], vec![]),
        (412, vec!["\"auth-+2\""], vec![]),
        (412, vec!["\"auth-1\""], vec![]),
        (412, vec!["\"auth-2\"", "\"auth-3\""], vec![]),
        (408, vec![], vec![]),
        (429, vec![], vec![]),
        (500, vec![], vec![]),
    ] {
        let (mut agent, generation, control) = connected(crate::TopicRegistrations::default());
        agent
            .command(AgentCommand::RenewAuthorization(
                "credential-not-in-state".into(),
            ))
            .unwrap();
        let Some(Effect::Http(HttpEffect::Request {
            operation,
            request: original,
            ..
        })) = agent.next_effect_after_turn()
        else {
            panic!("initial PATCH");
        };
        agent
            .command(AgentCommand::RenewAuthorization(
                "latest-not-in-state".into(),
            ))
            .unwrap();
        agent
            .handle(HostEvent::Http(HttpEvent::Response {
                operation,
                response: HttpResponse {
                    status,
                    headers: etags
                        .into_iter()
                        .map(|value| HttpHeader {
                            name: "ETag".into(),
                            value: value.into(),
                        })
                        .collect(),
                    body,
                },
            }))
            .unwrap();
        observe(&mut agent, generation, control, vec![grant(2)]);
        assert_eq!(agent.snapshot().accepted_credential_revision, Some(0));
        let outcome = agent.snapshot().credential_outcome.as_ref().unwrap();
        assert_eq!(outcome.revision, 1);
        assert_eq!(outcome.status, CredentialStatus::Uncertain);
        assert!(!format!("{:?}", agent.snapshot()).contains("not-in-state"));
        let Some(Effect::Timer(TimerEffect::Schedule { timer, after })) =
            agent.next_effect_after_turn()
        else {
            panic!("bounded retry");
        };
        assert!(!after.is_zero());
        agent
            .handle(HostEvent::Timer(TimerEvent::Fired { timer }))
            .unwrap();
        let Some(Effect::Http(HttpEffect::Request { request, .. })) =
            agent.next_effect_after_turn()
        else {
            panic!("retry PATCH");
        };
        assert_eq!(
            request, original,
            "status {status} cannot resolve the original request's uncertainty"
        );
    }

    let (mut agent, generation, control) = connected(crate::TopicRegistrations::default());
    agent
        .command(AgentCommand::RenewAuthorization("first".into()))
        .unwrap();
    let Some(Effect::Http(HttpEffect::Request { operation, .. })) = agent.next_effect_after_turn()
    else {
        panic!("first PATCH");
    };
    agent
        .command(AgentCommand::RenewAuthorization("second".into()))
        .unwrap();
    observe(&mut agent, generation, control, vec![grant(5)]);
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation,
            response: HttpResponse {
                status: 412,
                headers: vec![HttpHeader {
                    name: "ETag".into(),
                    value: "\"auth-4\"".into(),
                }],
                body: vec![],
            },
        }))
        .unwrap();
    agent
        .command(AgentCommand::RenewAuthorization("third".into()))
        .unwrap();
    let before = agent.snapshot().clone();
    agent
        .command(AgentCommand::RenewAuthorization("third".into()))
        .unwrap();
    assert_eq!(agent.snapshot(), &before);
    let Some(Effect::Timer(TimerEffect::Schedule { timer, after })) =
        agent.next_effect_after_turn()
    else {
        panic!("fenced retry");
    };
    assert!(!after.is_zero());
    agent
        .handle(HostEvent::Timer(TimerEvent::Fired { timer }))
        .unwrap();
    let Some(Effect::Http(HttpEffect::Request { request, .. })) = agent.next_effect_after_turn()
    else {
        panic!("coalesced PATCH");
    };
    assert!(
        request
            .headers
            .iter()
            .any(|header| header.name == "Authorization" && header.value == "Bearer third")
    );
    assert!(
        request
            .headers
            .iter()
            .any(|header| header.name == "If-Match" && header.value == "\"auth-5\"")
    );
    assert_eq!(agent.snapshot().credential_revision, 3);
    assert_eq!(agent.snapshot().accepted_credential_revision, Some(0));
    assert_eq!(
        agent
            .snapshot()
            .credential_outcome
            .as_ref()
            .unwrap()
            .revision,
        1
    );
}

#[test]
fn received_resource_state_projects_prejoin_and_owner_local_intent() {
    let mut agent = Agent::new(crate::AgentConfig {
        endpoint: "https://sfu.test/".into(),
        token: "token".into(),
        retry: Default::default(),
        log_level: Default::default(),
    })
    .unwrap();
    let selector = crate::TrackSelector {
        room: "main".into(),
        participant_id: "alice-device".into(),
        track_id: "camera".into(),
    };
    let mut claim_video = || {
        let crate::ResourceResult::Owned { owner } = agent
            .resource(crate::ResourceCommand::ClaimVideo {
                selector: selector.clone(),
            })
            .unwrap()
        else {
            panic!("expected video subscription owner")
        };
        owner
    };
    let first = claim_video();
    let second = claim_video();
    let first_initial = agent.video_subscription_state(first).unwrap();
    let second_initial = agent.video_subscription_state(second).unwrap();
    assert_eq!(first_initial.selector.participant_id, "alice-device");
    assert_eq!(first_initial.intent_revision, 1);
    assert!(!first_initial.local_enabled);
    assert_eq!(
        first_initial.source_status,
        crate::ffi::SubscriptionSourceStatus::Pending
    );
    assert_eq!(
        first_initial.admission,
        crate::ffi::PublicationAdmissionStatus::Pending
    );
    assert!(first_initial.admission_pending);
    assert!(!second_initial.local_enabled);

    agent
        .resource(crate::ResourceCommand::ObserveVideo {
            owner: first,
            observation: crate::VideoObservation {
                visible: true,
                ..Default::default()
            },
        })
        .unwrap();
    let first_changed = agent.video_subscription_state(first).unwrap();
    let second_unchanged = agent.video_subscription_state(second).unwrap();
    assert_eq!(first_changed.intent_revision, 2);
    assert!(first_changed.local_enabled);
    assert_eq!(second_unchanged.intent_revision, 1);
    assert!(!second_unchanged.local_enabled);

    let crate::ResourceResult::Owned { owner: publication } = agent
        .resource(crate::ResourceCommand::DeclareDataPublication {
            capture_clock: None,
            spec: crate::TrackSpec::Data {
                track_id: "events".into(),
                delivery: crate::TopicMode::Ordered,
            },
        })
        .unwrap()
    else {
        panic!("expected data publication owner")
    };
    let publication = agent.data_publication_state(publication).unwrap();
    assert_eq!(publication.intent_revision, 1);
    assert_eq!(
        publication.admission,
        crate::ffi::PublicationAdmissionStatus::Pending
    );
    assert!(publication.admission_pending);
    assert_eq!(
        publication.execution,
        crate::ffi::PublicationExecution::Unavailable
    );
}

#[test]
fn logical_offer_selects_exact_identity_without_discovery_and_gates_pending_disabled() {
    let (mut agent, generation, control) = connected(Default::default());
    let selector = crate::TrackSelector {
        room: "room".into(),
        participant_id: "external".into(),
        track_id: "camera".into(),
    };
    let resource = Resource::LogicalReception(MediaSlot::RemoteVideo(0));
    let spec = |enabled| {
        let mut spec = logical_subscription(
            &selector,
            state::TrackKind::Video,
            Some(state::resource_settings::Receive::Video(Default::default())),
            None,
        );
        spec.settings.as_mut().unwrap().enabled = Some(enabled);
        spec
    };
    let track = state::Track {
        publication_id: "lookup-only-source".into(),
        participant_id: selector.participant_id.clone(),
        room_id: selector.room.clone(),
        track_id: selector.track_id.clone(),
        kind: Some(state::track::Kind::Video(state::VideoTrack {
            format: Some(state::CodecFormat::from(state::codec_format::Codec::Vp8(
                state::Vp8Parameters {},
            ))),
        })),
    };

    let stage = |agent: &mut Agent, enabled| {
        agent
            .active
            .as_mut()
            .unwrap()
            .state
            .stage(resource.clone(), Some(spec(enabled)))
            .unwrap();
        let batch = state::codec::decode_client(
            &agent
                .active
                .as_mut()
                .unwrap()
                .state
                .next_frame()
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        (
            batch.records[0].owned,
            batch.records[0].desired.as_ref().unwrap().revision,
        )
    };
    let confirm_offer = |agent: &mut Agent, reference, revision, enabled| {
        observe(
            agent,
            generation,
            control,
            vec![confirmed(
                state::resource_key::Key::Owned(reference),
                revision,
                state::confirmed_state::Value::LogicalSubscription(Box::new(
                    state::LogicalSubscriptionState {
                        data_v1: None,
                        spec: Some(spec(enabled)),
                        source_revision: if enabled { 1 } else { 2 },
                        source: enabled.then(|| track.clone()),
                        execution: None,
                    },
                )),
            )],
        );
    };

    let (reference, revision) = stage(&mut agent, true);
    assert_eq!(
        selected(
            agent.active.as_ref().unwrap(),
            &selector,
            state::TrackKind::Video
        ),
        None
    );
    confirm_offer(&mut agent, reference, revision, true);
    let active = agent.active.as_ref().unwrap();
    assert!(active.state.tracks().is_empty());
    assert!(active.state.participants().is_empty());
    assert_eq!(
        selected(active, &selector, state::TrackKind::Video),
        Some(track.publication_id.as_str())
    );
    let desired = crate::DesiredState {
        video: vec![crate::VideoSubscription {
            slot: 0,
            publication_id: track.publication_id.clone(),
            selector: Some(selector.clone()),
            height: 0,
            min_height: 0,
            min_fps: 0,
            priority: 0,
        }],
        ..Default::default()
    };
    assert_eq!(
        source_room_for_track(active, &desired, &track.publication_id),
        Some(selector.room.clone())
    );
    assert_eq!(
        source_room_for_track(
            active,
            &crate::DesiredState::default(),
            &track.publication_id
        ),
        None
    );
    assert_eq!(
        selected(
            active,
            &crate::TrackSelector {
                room: "other-room".into(),
                ..selector.clone()
            },
            state::TrackKind::Video,
        ),
        None
    );
    assert_eq!(
        selected(
            active,
            &crate::TrackSelector {
                participant_id: "other-participant".into(),
                ..selector.clone()
            },
            state::TrackKind::Video,
        ),
        None
    );

    let (disabled_reference, disabled_revision) = stage(&mut agent, false);
    assert_eq!(
        selected(
            agent.active.as_ref().unwrap(),
            &selector,
            state::TrackKind::Video
        ),
        None
    );
    assert_eq!(disabled_reference, reference);
    confirm_offer(&mut agent, disabled_reference, disabled_revision, false);
    assert_eq!(
        selected(
            agent.active.as_ref().unwrap(),
            &selector,
            state::TrackKind::Video
        ),
        None
    );
}
