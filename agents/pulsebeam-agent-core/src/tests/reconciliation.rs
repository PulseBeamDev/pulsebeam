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
        .unwrap()
        + targets.len() as u64; // One prepared report per initial logical offer.
    let mut changed = native_media::scoped_desired(2);
    changed.video[0].height = 1080;
    changed.video[0].priority = 4;
    changed.audio.selector_delays.insert(
        changed.audio.selected[0].clone(),
        PlayoutDelay::Fixed {
            min_ms: 10,
            max_ms: 40,
        },
    );
    changed.publications[0].track_id = "temporary-camera".into();
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
        .command(AgentCommand::ReplaceDesired(native_media::scoped_desired(
            4,
        )))
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
    let mut latest = native_media::scoped_desired(5);
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
                track_id: "canceled-camera".into(),
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
                track_id: "extra-camera".into(),
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
fn registration_churn_does_not_reduce_admitted_units() {
    let (mut agent, generation, _, publication) =
        native_connection::connected_publisher(TopicMode::Lossy);
    let spec = TrackSpec::Data {
        track_id: "state".into(),
        delivery: TopicMode::Lossy,
    };
    agent.send_data(spec, vec![7; 1025]).unwrap();
    let (operation, channel, bytes) = next_data_record(&mut agent);
    assert_eq!(channel, publication.channel);
    let first = pulsebeam_proto::data_record::Record::decode(&bytes, None).unwrap();
    assert_eq!(first.bytes(), vec![7; 1024]);
    for (revision, publishers) in [
        (2, Vec::new()),
        (3, vec![publisher("state", TopicMode::Lossy)]),
    ] {
        agent
            .command(AgentCommand::ReplaceDesired(DesiredState {
                revision,
                connected: true,
                topics: TopicRegistrations {
                    publishers,
                    ..Default::default()
                },
                ..Default::default()
            }))
            .unwrap();
    }
    acknowledge_send(&mut agent, generation, channel, operation);
    let (operation, next, bytes) = next_data_record(&mut agent);
    assert_eq!(next, channel);
    let second = pulsebeam_proto::data_record::Record::decode(&bytes, None).unwrap();
    assert_eq!(second.bytes(), [7]);
    assert_eq!(second.header().unit_seq, first.header().unit_seq);
    assert_eq!(second.header().index, 1);
    acknowledge_send(&mut agent, generation, channel, operation);
    assert!(
        agent
            .next_effect_after_turn()
            .is_none_or(|effect| matches!(effect, Effect::Timer(TimerEffect::Cancel { .. })))
    );
}
