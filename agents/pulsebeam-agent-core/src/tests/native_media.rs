use super::*;
use crate::{client_sdp, rtc_preparation};
use pulsebeam_proto::state::{self, codec};

mod receiver;
pub(super) use receiver::{prepare_receivers, prepare_reception};

pub(super) struct Target {
    pub reference: u64,
    pub desired: state::DesiredState,
}

impl Target {
    pub fn spec(&self) -> &state::ResourceSpec {
        let Some(state::desired_state::Value::Present(spec)) = &self.desired.value else {
            panic!("present resource")
        };
        spec
    }

    pub fn receiver_installed(
        &self,
        participant: &state::Participant,
        tracks: &[state::Track],
    ) -> state::ServerStateRecord {
        let Some(state::resource_spec::Declaration::Subscription(subscription)) =
            &self.spec().declaration
        else {
            panic!("receiver")
        };
        let track = tracks
            .iter()
            .find(|track| track.track_id == subscription.track_id)
            .unwrap();
        self.installed(participant.clone(), track.clone())
    }

    pub fn receiver_preparing(
        &self,
        participant: &state::Participant,
        tracks: &[state::Track],
    ) -> state::ServerStateRecord {
        let mut record = self.receiver_installed(participant, tracks);
        let Some(state::confirmed_state::Value::Owned(live)) =
            record.confirmed.as_mut().unwrap().value.as_mut()
        else {
            panic!("receiver plan")
        };
        live.execution = state::ExecutionStatus::Preparing as i32;
        record
    }

    pub fn installed(
        &self,
        participant: state::Participant,
        track: state::Track,
    ) -> state::ServerStateRecord {
        native_connection::confirmed(
            state::resource_key::Key::Owned(self.reference),
            self.desired.revision,
            state::confirmed_state::Value::Owned(alloc::boxed::Box::new(state::LiveResource {
                spec: Some(self.spec().clone()),
                participant: Some(participant),
                track: Some(track),
                execution: if self.spec().settings.as_ref().unwrap().enabled == Some(true) {
                    state::ExecutionStatus::Installed
                } else {
                    state::ExecutionStatus::Disabled
                } as i32,
                execution_failure: None,
                binding: self.receive_plan(),
                activity: matches!(
                    self.spec().declaration,
                    Some(state::resource_spec::Declaration::Publication(_))
                )
                .then_some(state::MediaActivity::Idle as i32),
            })),
        )
    }
}

fn format(kind: MediaKind) -> state::CodecFormat {
    state::CodecFormat::from(match kind {
        MediaKind::Video => state::codec_format::Codec::Vp8(Default::default()),
        MediaKind::Audio => state::codec_format::Codec::Opus(Default::default()),
    })
}

pub(super) fn track(id: &str, participant: &str, label: &str, kind: MediaKind) -> state::Track {
    let format = Some(format(kind));
    state::Track {
        track_id: id.into(),
        participant_id: participant.into(),
        label: label.into(),
        kind: Some(match kind {
            MediaKind::Video => state::track::Kind::Video(state::VideoTrack { format }),
            MediaKind::Audio => state::track::Kind::Audio(state::AudioTrack { format }),
        }),
    }
}

pub(super) fn discover_receivers(
    agent: &mut Agent,
    generation: Generation,
    channel: ChannelId,
) -> (state::Participant, [state::Track; 2]) {
    let participant = state::Participant {
        participant_id: "publisher".into(),
        participant_external_id: "publisher-external".into(),
    };
    let tracks = [
        track("video-track", "publisher", "camera", MediaKind::Video),
        track("audio-track", "publisher", "microphone", MediaKind::Audio),
    ];
    let mut records = vec![native_connection::confirmed(
        state::resource_key::Key::Participant(participant.participant_id.clone()),
        0,
        state::confirmed_state::Value::Participant(participant.clone()),
    )];
    records.extend(tracks.iter().map(|track| {
        native_connection::confirmed(
            state::resource_key::Key::Track(track.track_id.clone()),
            0,
            state::confirmed_state::Value::Track(track.clone()),
        )
    }));
    native_connection::observe(agent, generation, channel, records);
    (participant, tracks)
}

#[track_caller]
pub(super) fn next(agent: &mut Agent, generation: Generation, channel: ChannelId) -> Vec<Target> {
    let Effect::DataChannel(DataChannelEffect::Send {
        operation,
        generation: actual_generation,
        channel: actual_channel,
        payload,
        ..
    }) = next_effect(agent)
    else {
        panic!("native resource frame")
    };
    assert_eq!((actual_generation, actual_channel), (generation, channel));
    let batch = codec::decode_client(&payload).unwrap();
    acknowledge_send(agent, generation, channel, operation);
    batch
        .records
        .into_iter()
        .map(|record| {
            let reference = record.owned;
            let Some(desired) = record.desired else {
                panic!("desired resource")
            };
            Target { reference, desired }
        })
        .collect()
}

fn current_bindings(updated: &[SlotBinding]) -> client_sdp::Bindings {
    let mut resources = resources(channel(9));
    resources
        .slots
        .retain(|binding| !updated.iter().any(|current| current.slot == binding.slot));
    resources.slots.extend_from_slice(updated);
    prepared_media(&resources).0
}

pub(super) fn finish_retirements(
    agent: &mut Agent,
    generation: Generation,
    expected: &[&str],
    updated: &[SlotBinding],
) -> Vec<OperationId> {
    let mut bindings = current_bindings(updated);
    let mut retired = Vec::new();
    let mut operations = Vec::new();
    for _ in expected {
        let effect = next_effect(agent);
        let Effect::Rtc(RtcEffect::RetireMedia {
            operation,
            generation: actual,
            mids,
        }) = effect
        else {
            panic!("media retirement, got {effect:?}")
        };
        assert_eq!(actual, generation);
        assert_eq!(mids.len(), 1);
        operations.push(operation);
        bindings
            .publications
            .retain(|binding| !mids.contains(&binding.mid));
        bindings
            .receptions
            .retain(|binding| !mids.contains(&binding.mid));
        retired.extend(mids);
        agent
            .handle(HostEvent::Rtc(RtcEvent::MediaPrepared {
                generation,
                operation,
                slots: Vec::new(),
                outcome: client_sdp::ApplyOutcome::Applied,
                preparation: Ok(bindings.clone()),
            }))
            .unwrap();
    }
    retired.sort();
    let mut expected = expected.to_vec();
    expected.sort();
    assert_eq!(retired, expected);
    operations
}

pub(super) fn finish_publication(
    agent: &mut Agent,
    generation: Generation,
    assigned: SlotBinding,
    retired: &[&str],
    updated: &[SlotBinding],
) {
    assert_eq!(assigned.direction, MediaDirection::SendOnly);
    let Effect::Rtc(RtcEffect::AllocateMedia {
        operation,
        generation: actual,
        allocations,
    }) = next_effect(agent)
    else {
        panic!("media allocation")
    };
    assert_eq!(actual, generation);
    assert_eq!(
        allocations,
        [client_sdp::Allocation {
            slot: assigned.slot.clone(),
            format: None,
        }]
    );
    let mut resources = resources(channel(9));
    let mut bindings = current_bindings(updated);
    bindings
        .publications
        .retain(|binding| !retired.contains(&binding.mid.as_str()));
    bindings
        .receptions
        .retain(|binding| !retired.contains(&binding.mid.as_str()));
    resources.slots = vec![assigned.clone()];
    let (fresh, _) = prepared_media(&resources);
    bindings.publications.extend(fresh.publications);
    bindings.receptions.extend(fresh.receptions);
    agent
        .handle(HostEvent::Rtc(RtcEvent::MediaPrepared {
            generation,
            operation,
            slots: vec![assigned],
            outcome: client_sdp::ApplyOutcome::Applied,
            preparation: Ok(bindings),
        }))
        .unwrap();
}

fn prepared_media(resources: &OfferResources) -> (client_sdp::Bindings, state::RtpCapabilities) {
    let mut bindings = client_sdp::Bindings::default();
    let mut capabilities = state::RtpCapabilities::default();
    for assigned in &resources.slots {
        let (mut binding, capability) = rtc_preparation::binding_fixture(format(assigned.kind));
        binding.key = rtc_preparation::bootstrap_media_key(&assigned.slot);
        binding.mid = assigned.mid.clone();
        binding.codec.payload_type = if assigned.kind == MediaKind::Video {
            102
        } else {
            111
        };
        let primary = 100 + assigned.media_index * 2;
        binding.encodings[0].primary = Some(primary);
        if binding.encodings[0].repair.is_some() {
            binding.encodings[0].repair = Some(primary + 1);
        }
        for codec in capability.codecs {
            if !capabilities.codecs.contains(&codec) {
                capabilities.codecs.push(codec);
            }
        }
        capabilities.header_extensions = capability.header_extensions;
        capabilities.rtcp_modes = capability.rtcp_modes;
        match assigned.direction {
            MediaDirection::SendOnly => bindings.publications.push(binding),
            MediaDirection::ReceiveOnly => bindings.receptions.push(binding),
        }
    }
    (bindings, capabilities)
}

pub(super) fn connected() -> (Agent, Generation, ChannelId, Vec<Target>) {
    connected_with(Agent::new(config()).unwrap(), desired(1))
}

pub(super) fn begin(
    mut agent: Agent,
    initial: DesiredState,
    prepare: impl FnOnce(&mut client_sdp::Bindings),
) -> (Agent, Generation, ChannelId) {
    let channel = channel(9);
    let resources = resources(channel);
    let (mut bindings, capabilities) = prepared_media(&resources);
    prepare(&mut bindings);
    let offer = offer()
        .replace("m=video 9 UDP/TLS/RTP/SAVPF 102", "m=video 9 UDP/TLS/RTP/SAVPF 102 97\r\na=rtpmap:102 VP8/90000\r\na=rtpmap:97 rtx/90000\r\na=fmtp:97 apt=102\r\na=rtcp-fb:102 transport-cc\r\na=extmap:1 urn:ietf:params:rtp-hdrext:sdes:mid\r\na=extmap:20 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01")
        .replace("m=audio 9 UDP/TLS/RTP/SAVPF 111", "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\na=rtpmap:111 opus/48000/2\r\na=rtcp-fb:111 transport-cc\r\na=extmap:1 urn:ietf:params:rtp-hdrext:sdes:mid\r\na=extmap:20 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01");
    agent
        .command(AgentCommand::ReplaceDesired(initial))
        .unwrap();
    let Effect::Rtc(RtcEffect::CreateOffer { generation, .. }) = next_effect(&mut agent) else {
        panic!("offer")
    };
    agent
        .handle(HostEvent::Rtc(RtcEvent::OfferCreated {
            generation,
            offer,
            resources,
        }))
        .unwrap();
    let Effect::Http(HttpEffect::Request { operation, .. }) = next_effect(&mut agent) else {
        panic!("POST")
    };
    let mut response = create_response("p1", "unused", PARTICIPANT_URI);
    let mut body: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
    body["rtp_capabilities"] = serde_json::to_value(&capabilities).unwrap();
    response.body = serde_json::to_vec(&body).unwrap();
    agent
        .handle(HostEvent::Http(HttpEvent::Response {
            operation,
            response,
        }))
        .unwrap();
    let Effect::Rtc(RtcEffect::ApplyAnswer {
        capabilities: accepted,
        ..
    }) = next_effect(&mut agent)
    else {
        panic!("answer")
    };
    assert_eq!(accepted.rtp, capabilities);
    agent
        .handle(HostEvent::Rtc(RtcEvent::BootstrapMediaPrepared {
            generation,
            preparation: Ok(bindings),
        }))
        .unwrap();
    for event in [
        HostEvent::Rtc(RtcEvent::AnswerApplied { generation }),
        HostEvent::Rtc(RtcEvent::Connected { generation }),
        HostEvent::DataChannel(DataChannelEvent::Opened {
            generation,
            channel,
        }),
    ] {
        agent.handle(event).unwrap();
    }
    native_connection::observe(
        &mut agent,
        generation,
        channel,
        vec![native_connection::grant(1)],
    );
    assert_eq!(agent.snapshot().connection, ConnectionState::Connected);
    (agent, generation, channel)
}

pub(super) fn connected_with(
    agent: Agent,
    initial: DesiredState,
) -> (Agent, Generation, ChannelId, Vec<Target>) {
    let (mut agent, generation, channel) = begin(agent, initial, |_| {});
    let mut targets = next(&mut agent, generation, channel);
    let publication = targets.remove(
        targets
            .iter()
            .position(|target| {
                matches!(
                    target.spec().declaration,
                    Some(state::resource_spec::Declaration::Publication(_))
                )
            })
            .unwrap(),
    );
    let participant = state::Participant {
        participant_id: agent.snapshot().participant_id.clone().unwrap(),
        participant_external_id: agent.snapshot().participant_external_id.clone().unwrap(),
    };
    native_connection::observe(
        &mut agent,
        generation,
        channel,
        vec![publication.installed(
            participant.clone(),
            track(
                "local-camera-source",
                &participant.participant_id,
                "camera",
                MediaKind::Video,
            ),
        )],
    );
    assert_eq!(agent.snapshot().connection, ConnectionState::Connected);
    assert!(agent.next_effect().is_none());
    (agent, generation, channel, targets)
}
