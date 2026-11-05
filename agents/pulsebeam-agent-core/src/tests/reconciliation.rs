use super::*;

fn wake(agent: &mut Agent) -> TimerId {
    let Some(Effect::Timer(TimerEffect::Schedule { timer, after })) = agent.next_effect() else {
        panic!("one reconciliation wake")
    };
    assert!(after.is_zero());
    assert!(agent.next_effect().is_none(), "no work before the boundary");
    timer
}

fn fire(agent: &mut Agent, timer: TimerId) {
    agent
        .handle(HostEvent::Timer(TimerEvent::Fired { timer }))
        .unwrap();
}

#[test]
fn whole_state_churn_reduces_once_without_physical_work_or_wire_revisions() {
    let (mut agent, generation, channel, targets) = native_media::connected();
    let (participant, tracks) = native_media::discover_receivers(&mut agent, generation, channel);
    native_connection::observe(
        &mut agent,
        generation,
        channel,
        targets
            .iter()
            .map(|target| target.receiver_installed(&participant, &tracks))
            .collect(),
    );
    assert!(agent.next_effect_after_turn().is_none());
    let revision = targets
        .iter()
        .map(|target| target.desired.revision)
        .max()
        .unwrap();
    let mut changed = desired(2);
    changed.video[0].height = 1080;
    changed.video[0].priority = 4;
    changed.audio.playout_delays.insert(
        "audio-track".into(),
        PlayoutDelay::Fixed {
            min_ms: 10,
            max_ms: 40,
        },
    );
    changed.publications[0].label = "temporary-camera".into();
    changed
        .topics
        .publishers
        .push(publisher("temporary-topic", TopicMode::Ordered));
    agent
        .command(AgentCommand::ReplaceDesired(changed.clone()))
        .unwrap();
    let timer = wake(&mut agent);
    changed.revision = 3;
    changed.video.clear();
    changed.audio = AudioSubscription::default();
    changed.publications.clear();
    changed.topics = TopicRegistrations::default();
    agent
        .command(AgentCommand::ReplaceDesired(changed))
        .unwrap();
    agent
        .command(AgentCommand::ReplaceDesired(desired(4)))
        .unwrap();
    assert_eq!(agent.snapshot().desired_revision, 4);
    assert!(
        agent.next_effect().is_none(),
        "later intent neither resets nor adds a wake"
    );
    fire(&mut agent, timer);
    assert!(
        agent.next_effect().is_none(),
        "valid applied A needs no work"
    );
    for _ in 0..3 {
        fire(&mut agent, timer);
        native_connection::observe(
            &mut agent,
            generation,
            channel,
            targets
                .iter()
                .map(|target| target.receiver_installed(&participant, &tracks))
                .collect(),
        );
        assert!(
            agent.next_effect().is_none(),
            "stale wakes and duplicate facts are quiescent"
        );
    }
    let mut latest = desired(5);
    latest.video[0].height = 540;
    agent.command(AgentCommand::ReplaceDesired(latest)).unwrap();
    let timer = wake(&mut agent);
    fire(&mut agent, timer);
    let update = native_media::next(&mut agent, generation, channel);
    assert_eq!(update.len(), 1);
    assert_eq!(
        update[0].desired.revision,
        revision + 1,
        "intermediate targets consume no wire revisions"
    );
    assert!(
        targets
            .iter()
            .any(|target| target.reference == update[0].reference)
    );
    assert!(agent.next_effect_after_turn().is_none());
}

#[test]
fn creation_release_before_first_boundary_has_no_physical_or_wire_lifetime() {
    let (mut agent, _, _) = native_connection::connected(Default::default());
    let ResourceResult::Publication { owner, .. } = agent
        .resource(ResourceCommand::ClaimPublication {
            spec: TrackSpec::Video {
                label: "canceled-camera".into(),
            },
        })
        .unwrap()
    else {
        panic!("publication owner")
    };
    agent
        .resource(ResourceCommand::UpdatePublication {
            owner,
            present: true,
        })
        .unwrap();
    let timer = wake(&mut agent);
    agent.resource(ResourceCommand::Release { owner }).unwrap();
    assert!(matches!(
        agent.resource(ResourceCommand::UpdatePublication {
            owner,
            present: true
        }),
        Err(AgentError::InvalidResource(TrackResourceError::Released))
    ));
    assert!(agent.next_effect().is_none());
    fire(&mut agent, timer);
    assert!(agent.next_effect().is_none());
    fire(&mut agent, timer);
    assert!(agent.next_effect().is_none());
}

#[test]
fn released_preparation_cannot_install_or_serialize_its_late_success() {
    use crate::{
        client_sdp::{ApplyOutcome, Bindings},
        rtc_preparation,
    };
    use pulsebeam_proto::state;
    let (mut agent, generation, _, _) = native_media::connected();
    let ResourceResult::Publication { owner, slot, .. } = agent
        .resource(ResourceCommand::ClaimPublication {
            spec: TrackSpec::Video {
                label: "extra-camera".into(),
            },
        })
        .unwrap()
    else {
        panic!("publication owner")
    };
    agent
        .resource(ResourceCommand::UpdatePublication {
            owner,
            present: true,
        })
        .unwrap();
    let slot = MediaSlot::LocalVideo(slot);
    let timer = wake(&mut agent);
    fire(&mut agent, timer);
    let Some(Effect::Rtc(RtcEffect::AllocateMedia {
        operation,
        allocations,
        ..
    })) = agent.next_effect()
    else {
        panic!("physical preparation")
    };
    assert_eq!(allocations.len(), 1);
    assert_eq!(allocations[0].slot, slot);
    assert!(agent.next_effect().is_none());
    agent.resource(ResourceCommand::Release { owner }).unwrap();
    let timer = wake(&mut agent);
    let (mut binding, _) = rtc_preparation::binding_fixture(state::CodecFormat::from(
        state::codec_format::Codec::Vp8(Default::default()),
    ));
    binding.key = rtc_preparation::bootstrap_media_key(&slot);
    binding.mid = "late-preparation".into();
    let assigned = SlotBinding {
        slot: slot.clone(),
        mid: binding.mid.clone(),
        media_index: 1,
        kind: slot.kind(),
        direction: slot.direction(),
    };
    agent
        .handle(HostEvent::Rtc(RtcEvent::MediaPrepared {
            generation,
            operation,
            slots: vec![assigned],
            outcome: ApplyOutcome::Applied,
            preparation: Ok(Bindings {
                publications: vec![binding],
                receptions: Vec::new(),
            }),
        }))
        .unwrap();
    assert!(agent.next_effect().is_none());
    fire(&mut agent, timer);
    let Some(Effect::Rtc(RtcEffect::RetireMedia {
        operation, mids, ..
    })) = agent.next_effect()
    else {
        panic!("late physical lifetime must retire")
    };
    assert_eq!(mids, ["late-preparation"]);
    assert!(
        agent.next_effect().is_none(),
        "never serialized the released owner"
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
    let timer = wake(&mut agent);
    fire(&mut agent, timer);
    assert!(
        agent.next_effect().is_none(),
        "no replacement or retry loop"
    );
}

#[test]
fn registration_churn_does_not_reduce_ordered_payload_operations() {
    let (mut agent, generation, _, publication) =
        native_connection::connected_publisher(TopicMode::Ordered);
    let send = |bytes: &[u8]| {
        AgentCommand::SendTopic(TopicSend {
            publisher: publisher("state", TopicMode::Ordered),
            payload: bytes.to_vec(),
        })
    };
    agent.command(send(b"first")).unwrap();
    agent.command(send(b"second")).unwrap();
    let Some(Effect::DataChannel(DataChannelEffect::Send {
        operation,
        channel,
        payload,
        ..
    })) = agent.next_effect()
    else {
        panic!("first payload operation")
    };
    assert_eq!(channel, publication.channel);
    let Some(TopicBody::Message(first)) = TopicFrame::decode_bounded(&payload).unwrap().body else {
        panic!("first message")
    };
    assert_eq!(first.payload, b"first");
    agent
        .command(AgentCommand::ReplaceDesired(DesiredState {
            revision: 2,
            connected: true,
            ..Default::default()
        }))
        .unwrap();
    let timer = wake(&mut agent);
    assert!(matches!(
        agent.command(send(b"not-registered")),
        Err(AgentError::InvalidTopic(
            TopicError::PublisherNotRegistered(_)
        ))
    ));
    agent
        .command(AgentCommand::ReplaceDesired(DesiredState {
            revision: 3,
            connected: true,
            topics: TopicRegistrations {
                publishers: vec![publisher("state", TopicMode::Ordered)],
                subscribers: Vec::new(),
            },
            ..Default::default()
        }))
        .unwrap();
    acknowledge_send(&mut agent, generation, channel, operation);
    let Some(Effect::DataChannel(DataChannelEffect::Send {
        operation,
        channel: next,
        payload,
        ..
    })) = agent.next_effect()
    else {
        panic!("second payload operation")
    };
    assert_eq!(next, channel);
    let Some(TopicBody::Message(second)) = TopicFrame::decode_bounded(&payload).unwrap().body
    else {
        panic!("second message")
    };
    assert_eq!(second.payload, b"second");
    assert_eq!(second.sequence, first.sequence + 1);
    acknowledge_send(&mut agent, generation, channel, operation);
    fire(&mut agent, timer);
    assert!(
        agent.next_effect().is_none(),
        "registration target is unchanged, with no control or physical work"
    );
    assert_eq!(agent.snapshot().topics.accepted_sends, 2);
    assert_eq!(agent.snapshot().topics.dropped_sends, 1);
    assert_eq!(agent.snapshot().topics.publishers[0].accepted_history, 2);
}
