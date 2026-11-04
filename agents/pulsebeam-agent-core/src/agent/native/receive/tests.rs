use super::*;
use crate::client_sdp::{ApplyOutcome, Bindings};
use crate::tests::native_connection::{confirmed, connected, observe};
use state::codec;

fn frame(agent: &mut Agent, generation: Generation, channel: ChannelId) -> state::ClientStateBatch {
    let Some(Effect::DataChannel(DataChannelEffect::Send {
        operation,
        generation: actual,
        channel: actual_channel,
        payload,
        ..
    })) = agent.next_effect()
    else {
        panic!("control frame")
    };
    assert_eq!((actual, actual_channel), (generation, channel));
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Sent {
            generation,
            channel,
            operation,
        }))
        .unwrap();
    codec::decode_client(&payload).unwrap()
}

#[test]
fn receive_plan_waits_for_correlated_platform_completion_and_reports_separately() {
    let (mut agent, generation, channel) = connected(crate::TopicRegistrations::default());
    let format = state::CodecFormat::from(state::codec_format::Codec::Vp8(Default::default()));
    let (mut binding, capabilities) = crate::rtc_preparation::binding_fixture(format.clone());
    agent.active.as_mut().unwrap().capabilities.rtp = capabilities.clone();
    let slot = SlotBinding {
        slot: MediaSlot::RemoteVideo(0),
        mid: "actual".into(),
        media_index: 1,
        kind: crate::MediaKind::Video,
        direction: crate::MediaDirection::ReceiveOnly,
    };
    binding.key = crate::rtc_preparation::bootstrap_media_key(&slot.slot);
    binding.mid = slot.mid.clone();
    binding.codec.payload_type = 96;
    binding.encodings[0].primary = Some(100);
    binding.encodings[0].repair = Some(101);
    let offer_binding = Bindings {
        publications: Vec::new(),
        receptions: vec![binding.clone()],
    };
    let offered = crate::rtc_preparation::prepare_bootstrap_bindings(
        offer_binding.clone(),
        &[slot.clone()],
        &capabilities,
    )
    .unwrap();
    let Some(state::track_binding::Transport::Rtp(parameters)) = offered[&slot.slot]
        .as_ref()
        .unwrap()
        .binding
        .transport
        .as_ref()
    else {
        panic!("RTP")
    };
    let offer = state::RtpReceiveOffer {
        mid: slot.mid.clone(),
        codecs: vec![parameters.codec.clone().unwrap()],
        header_extensions: parameters.header_extensions.clone(),
        rtcp_modes: vec![state::RtcpMode::Compound.into()],
    };
    let participant = state::Participant {
        participant_id: "publisher".into(),
        participant_external_id: "publisher-external".into(),
    };
    let track = state::Track {
        track_id: "source".into(),
        participant_id: participant.participant_id.clone(),
        label: "camera".into(),
        kind: Some(state::track::Kind::Video(state::VideoTrack {
            format: Some(format.clone()),
        })),
    };
    observe(
        &mut agent,
        generation,
        channel,
        vec![
            confirmed(
                state::resource_key::Key::Participant(participant.participant_id.clone()),
                0,
                state::confirmed_state::Value::Participant(participant.clone()),
            ),
            confirmed(
                state::resource_key::Key::Track(track.track_id.clone()),
                0,
                state::confirmed_state::Value::Track(track.clone()),
            ),
        ],
    );
    agent
        .command(AgentCommand::ReplaceDesired(DesiredState {
            revision: 2,
            connected: true,
            video: vec![crate::VideoSubscription {
                slot: 0,
                track_id: track.track_id.clone(),
                selector: None,
                height: 0,
                min_height: 0,
                min_fps: 0,
                priority: 0,
                playout_delay: crate::PlayoutDelay::Adaptive,
            }],
            ..Default::default()
        }))
        .unwrap();
    let Some(Effect::Rtc(RtcEffect::OfferReceiveMedia {
        generation: actual,
        operation,
        target,
    })) = agent.next_effect()
    else {
        panic!("receive offer effect")
    };
    assert_eq!(actual, generation);
    assert_eq!(
        target,
        crate::ReceiveTarget::Allocate(crate::client_sdp::Allocation {
            slot: slot.slot.clone(),
            format: Some(format)
        })
    );
    agent
        .handle(HostEvent::Rtc(RtcEvent::MediaPrepared {
            generation,
            operation,
            slots: vec![slot.clone()],
            outcome: ApplyOutcome::Applied,
            preparation: Ok(offer_binding),
        }))
        .unwrap();
    assert!(agent.active.as_ref().unwrap().media_allocation.is_some());
    assert!(agent.next_effect().is_none());
    let wrong = agent.ids.operation();
    let stale = agent.ids.generation();
    for (event_generation, event_operation) in [(stale, operation), (generation, wrong)] {
        agent
            .handle(HostEvent::Rtc(RtcEvent::MediaOffered {
                generation: event_generation,
                operation: event_operation,
                slot: slot.clone(),
                offer: offer.clone(),
            }))
            .unwrap();
        assert!(agent.next_effect().is_none());
    }
    agent
        .handle(HostEvent::Rtc(RtcEvent::MediaOffered {
            generation,
            operation,
            slot: slot.clone(),
            offer: offer.clone(),
        }))
        .unwrap();
    let target = frame(&mut agent, generation, channel).records.remove(0);
    let desired = target.desired.unwrap();
    let Some(state::desired_state::Value::Present(spec)) = desired.value else {
        panic!("receive desire")
    };
    let Some(state::resource_spec::Declaration::Subscription(subscription)) = &spec.declaration
    else {
        panic!("subscription")
    };
    assert_eq!(subscription.rtp_offer.as_ref(), Some(&offer));
    assert!(subscription.binding.is_none());
    assert!(agent.snapshot.video_mapping.is_empty());
    assert!(agent.next_effect().is_none());

    binding.encodings[0].primary = Some(9000);
    binding.encodings[0].repair = Some(9001);
    binding.rtcp.cname = Some("server-selected".into());
    binding.extensions.pop();
    let applied = Bindings {
        publications: Vec::new(),
        receptions: vec![binding],
    };
    let prepared = crate::rtc_preparation::prepare_bootstrap_bindings(
        applied.clone(),
        &[slot.clone()],
        &capabilities,
    )
    .unwrap();
    let selected = prepared[&slot.slot].as_ref().unwrap().binding.clone();
    let mut live = state::LiveResource {
        spec: Some(*spec),
        participant: Some(participant),
        track: Some(track),
        execution: state::ExecutionStatus::Preparing as i32,
        execution_failure: None,
        binding: Some(selected.clone()),
        activity: None,
    };
    observe(
        &mut agent,
        generation,
        channel,
        vec![confirmed(
            state::resource_key::Key::Owned(target.owned),
            desired.revision,
            state::confirmed_state::Value::Owned(alloc::boxed::Box::new(live.clone())),
        )],
    );
    let Some(Effect::Rtc(RtcEffect::ApplyReceivePlan {
        generation: actual,
        operation: selected_operation,
        slot: actual_slot,
        parameters,
    })) = agent.next_effect()
    else {
        panic!("apply receive plan")
    };
    assert_eq!(
        (actual, selected_operation, actual_slot),
        (generation, operation, slot.clone())
    );
    assert_eq!(
        Some(state::track_binding::Transport::Rtp(parameters)),
        selected.transport
    );
    assert!(agent.next_effect().is_none());
    live.execution = state::ExecutionStatus::Installed as i32;
    observe(
        &mut agent,
        generation,
        channel,
        vec![confirmed(
            state::resource_key::Key::Owned(target.owned),
            desired.revision,
            state::confirmed_state::Value::Owned(alloc::boxed::Box::new(live)),
        )],
    );
    assert!(
        agent.snapshot.video_mapping.is_empty(),
        "server status alone is not local installation evidence"
    );
    assert!(agent.media_observation_target(&slot.slot).is_none());
    assert!(agent.next_effect().is_none());
    agent
        .handle(HostEvent::Rtc(RtcEvent::MediaPrepared {
            generation,
            operation: wrong,
            slots: vec![slot.clone()],
            outcome: ApplyOutcome::Applied,
            preparation: Ok(applied.clone()),
        }))
        .unwrap();
    assert!(agent.next_effect().is_none());
    agent
        .handle(HostEvent::Rtc(RtcEvent::MediaPrepared {
            generation,
            operation,
            slots: vec![slot.clone()],
            outcome: ApplyOutcome::Applied,
            preparation: Ok(applied),
        }))
        .unwrap();
    let reports = frame(&mut agent, generation, channel);
    assert_eq!(reports.records.len(), 1);
    let report = &reports.records[0];
    assert_eq!(report.owned, target.owned);
    assert!(report.desired.is_none());
    let report = report.execution.as_ref().unwrap();
    assert!(report.revision > desired.revision);
    assert_eq!(
        report.outcome,
        Some(state::execution_report::Outcome::Installed(selected))
    );
    assert_eq!(agent.snapshot.video_mapping[&1], "source");
    assert!(agent.media_observation_target(&slot.slot).is_some());
    assert!(agent.next_effect().is_none());
}
