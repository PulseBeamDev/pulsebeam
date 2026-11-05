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
        let want = |label: &str| MediaWant {
            resource: if receiving {
                Resource::Reception(slot.clone())
            } else {
                Resource::Publication {
                    kind: if slot.kind() == crate::MediaKind::Video {
                        state::TrackKind::Video
                    } else {
                        state::TrackKind::Audio
                    },
                    label: label.into(),
                }
            },
            source: receiving.then(|| label.into()),
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
            assert!(rtp(active, &resource, &slot, None).is_none());
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
            assert!(rtp(active, &resource, &slot, None).is_none());
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
                    state::resource_key::Key::Track(id.into()),
                    0,
                    state::confirmed_state::Value::Track(state::Track {
                        track_id: id.into(),
                        participant_id: "remote".into(),
                        label: "voice".into(),
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
        label: "camera".into(),
    };
    let cached = |mid: &str| crate::rtc_preparation::PreparedRtp {
        binding: state::TrackBinding {
            transport: Some(state::track_binding::Transport::Rtp(state::RtpParameters {
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
fn active_control_declares_only_prepared_payloads_and_retries_the_exact_serialized_frame() {
    let publisher = crate::TopicPublisher {
        topic: "literal/Δ label".into(),
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
    assert!(specification.ordered);
    assert!(agent.next_effect_after_turn().is_none());
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
    assert!(
        agent.next_effect_after_turn().is_none(),
        "requested properties cannot override incompatible host facts"
    );
    assert!(
        agent.snapshot().topics.publishers[0]
            .data_track_id
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
    assert!(specification.ordered);
    assert_eq!(specification.reliability, DataChannelReliability::Reliable);
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
                ordered: true,
                reliability: DataChannelReliability::Reliable,
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
                ordered: true,
                reliability: DataChannelReliability::Reliable,
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
    assert_eq!(declaration.label, publisher.topic);
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
                spec: Some(spec.as_ref().clone()),
                participant: Some(state::Participant {
                    participant_id: "p1".into(),
                    participant_external_id: "participant".into(),
                }),
                track: Some(state::Track {
                    track_id: "server-canonical-source".into(),
                    participant_id: "p1".into(),
                    label: publisher.topic.clone(),
                    kind: Some(state::track::Kind::Data(state::DataTrack {
                        delivery: state::DataDelivery::Ordered as i32,
                    })),
                }),
                execution: state::ExecutionStatus::Installed as i32,
                execution_failure: None,
                activity: None,
            })),
        )],
    );
    assert!(!agent.topics.has_channel(generation, channel));
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
    let (mut agent, generation, control) = connected(Default::default());
    agent
        .command(AgentCommand::RenewAuthorization("replacement".into()))
        .unwrap();
    let Some(Effect::Http(HttpEffect::Request {
        operation, request, ..
    })) = agent.next_effect_after_turn()
    else {
        panic!("PATCH")
    };
    assert_eq!(request.method, HttpMethod::Patch);
    assert!(request.body.is_empty());
    assert!(
        request
            .headers
            .iter()
            .any(|header| header.name == "If-Match" && header.value == "\"auth-1\"")
    );
    assert!(!format!("{request:?}").contains("replacement"));
    agent
        .handle(HostEvent::Http(HttpEvent::Failed {
            operation,
            message: "ambiguous".into(),
        }))
        .unwrap();
    observe(&mut agent, generation, control, vec![grant(2)]);
    assert_eq!(agent.config.token, "private-token");
    agent
        .command(AgentCommand::RenewAuthorization("replacement".into()))
        .unwrap();
    let Some(Effect::Http(HttpEffect::Request {
        operation, request, ..
    })) = agent.next_effect_after_turn()
    else {
        panic!("retry PATCH")
    };
    assert!(
        request
            .headers
            .iter()
            .any(|header| header.name == "If-Match" && header.value == "\"auth-1\"")
    );
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation,
            response: HttpResponse {
                status: 412,
                headers: Vec::new(),
                body: Vec::new(),
            },
        }))
        .unwrap();
    assert_eq!(agent.config.token, "private-token");
    agent
        .command(AgentCommand::RenewAuthorization("replacement".into()))
        .unwrap();
    let Some(Effect::Http(HttpEffect::Request {
        operation, request, ..
    })) = agent.next_effect_after_turn()
    else {
        panic!("fresh PATCH")
    };
    assert!(
        request
            .headers
            .iter()
            .any(|header| header.name == "If-Match" && header.value == "\"auth-2\"")
    );
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation,
            response: HttpResponse {
                status: 204,
                headers: vec![HttpHeader {
                    name: "ETag".into(),
                    value: "\"auth-3\"".into(),
                }],
                body: Vec::new(),
            },
        }))
        .unwrap();
    assert_eq!(agent.config.token, "private-token");
    observe(&mut agent, generation, control, vec![grant(3)]);
    assert_eq!(agent.config.token, "replacement");
}

#[test]
fn selectors_do_not_choose_a_source_from_ambiguous_labels() {
    let (mut agent, generation, control) = connected(Default::default());
    let participant = state::Participant {
        participant_id: "remote".into(),
        participant_external_id: "external".into(),
    };
    let track = state::Track {
        track_id: "source-z".into(),
        participant_id: participant.participant_id.clone(),
        label: "camera".into(),
        kind: Some(state::track::Kind::Video(state::VideoTrack {
            format: Some(state::CodecFormat::from(state::codec_format::Codec::Vp8(
                state::Vp8Parameters {},
            ))),
        })),
    };
    let mut other = track.clone();
    other.track_id = "source-a".into();
    observe(
        &mut agent,
        generation,
        control,
        vec![
            confirmed(
                state::resource_key::Key::Participant(participant.participant_id.clone()),
                0,
                state::confirmed_state::Value::Participant(participant),
            ),
            confirmed(
                state::resource_key::Key::Track(track.track_id.clone()),
                0,
                state::confirmed_state::Value::Track(track.clone()),
            ),
            confirmed(
                state::resource_key::Key::Track(other.track_id.clone()),
                0,
                state::confirmed_state::Value::Track(other.clone()),
            ),
        ],
    );
    let selector = crate::TrackSelector {
        participant_external_id: "external".into(),
        label: "camera".into(),
    };
    let resource = Resource::Reception(MediaSlot::RemoteVideo(0));
    assert_eq!(
        selected(
            agent.active.as_ref().unwrap(),
            &resource,
            &selector,
            state::TrackKind::Video
        ),
        None
    );
    observe(
        &mut agent,
        generation,
        control,
        vec![confirmed(
            state::resource_key::Key::Track(track.track_id),
            0,
            state::confirmed_state::Value::Absent(state::AbsentState {
                reason: state::AbsenceReason::Removed as i32,
            }),
        )],
    );
    assert_eq!(
        selected(
            agent.active.as_ref().unwrap(),
            &resource,
            &selector,
            state::TrackKind::Video
        ),
        Some(other.track_id.as_str())
    );
    let mut renamed = other.clone();
    renamed.track_id = "source-renamed".into();
    renamed.label = "screen".into();
    observe(
        &mut agent,
        generation,
        control,
        vec![
            confirmed(
                state::resource_key::Key::Track(other.track_id.clone()),
                0,
                state::confirmed_state::Value::Absent(state::AbsentState {
                    reason: state::AbsenceReason::Removed as i32,
                }),
            ),
            confirmed(
                state::resource_key::Key::Track(renamed.track_id.clone()),
                0,
                state::confirmed_state::Value::Track(renamed),
            ),
        ],
    );
    assert_eq!(
        selected(
            agent.active.as_ref().unwrap(),
            &resource,
            &selector,
            state::TrackKind::Video
        ),
        None
    );
    let mut republished = other;
    republished.track_id = "source-republished".into();
    observe(
        &mut agent,
        generation,
        control,
        vec![confirmed(
            state::resource_key::Key::Track(republished.track_id.clone()),
            0,
            state::confirmed_state::Value::Track(republished.clone()),
        )],
    );
    assert_eq!(
        selected(
            agent.active.as_ref().unwrap(),
            &resource,
            &selector,
            state::TrackKind::Video
        ),
        Some(republished.track_id.as_str())
    );
}
