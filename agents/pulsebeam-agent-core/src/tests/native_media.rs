use super::*;
use crate::{client_sdp, rtc_preparation};
use pulsebeam_proto::state::{self, codec};

#[derive(Clone)]
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
    next_if_ready(agent, generation, channel).expect("native resource frame")
}

fn next_if_ready(
    agent: &mut Agent,
    generation: Generation,
    channel: ChannelId,
) -> Option<Vec<Target>> {
    let effect = agent.next_effect_after_turn()?;
    let Effect::DataChannel(DataChannelEffect::Send {
        operation,
        generation: actual_generation,
        channel: actual_channel,
        payload,
        ..
    }) = effect
    else {
        panic!("native resource frame, got {effect:?}")
    };
    assert_eq!((actual_generation, actual_channel), (generation, channel));
    let batch = codec::decode_client(&payload).unwrap();
    acknowledge_send(agent, generation, channel, operation);
    Some(
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
            .collect(),
    )
}

fn current_bindings(updated: &[SlotBinding]) -> client_sdp::Bindings {
    let mut resources = resources(channel(9));
    resources
        .slots
        .retain(|binding| !updated.iter().any(|current| current.slot == binding.slot));
    resources.slots.extend_from_slice(updated);
    prepared_media(&resources).0
}

pub(super) fn confirm_missing_lookups(
    agent: &mut Agent,
    generation: Generation,
    channel: ChannelId,
) {
    let targets = next(agent, generation, channel);
    let records = targets
        .iter()
        .map(|target| {
            let value = match &target.desired.value {
                Some(state::desired_state::Value::Absent(true)) => {
                    state::confirmed_state::Value::Absent(state::AbsentState {
                        reason: state::AbsenceReason::Removed as i32,
                    })
                }
                Some(state::desired_state::Value::Present(spec))
                    if matches!(
                        spec.declaration,
                        Some(state::resource_spec::Declaration::SourceLookup(_))
                    ) =>
                {
                    state::confirmed_state::Value::SourceLookup(Box::new(
                        state::SourceLookupState {
                            spec: Some(spec.as_ref().clone()),
                            participant: None,
                            track: None,
                        },
                    ))
                }
                value => panic!("unexpected unresolved source work: {value:?}"),
            };
            native_connection::confirmed(
                state::resource_key::Key::Owned(target.reference),
                target.desired.revision,
                value,
            )
        })
        .collect();
    native_connection::observe(agent, generation, channel, records);
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

pub(super) fn finish_allocation(
    agent: &mut Agent,
    generation: Generation,
    assigned: SlotBinding,
    retired: &[&str],
    updated: &[SlotBinding],
) {
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
            format: (assigned.direction == MediaDirection::ReceiveOnly)
                .then(|| format(assigned.kind)),
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

pub(super) fn prepared_media(
    resources: &OfferResources,
) -> (client_sdp::Bindings, state::RtpCapabilities) {
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

pub(super) fn scoped_desired(revision: u64) -> DesiredState {
    let mut state = desired(revision);
    state.video[0].track_id.clear();
    state.video[0].selector = Some(TrackSelector {
        room: "room".into(),
        participant_external_id: "publisher-external".into(),
        label: "camera".into(),
    });
    state.audio.pinned.clear();
    state.audio.selected = vec![TrackSelector {
        room: "room".into(),
        participant_external_id: "publisher-external".into(),
        label: "microphone".into(),
    }];
    state
}

pub(super) fn connected() -> (Agent, Generation, ChannelId, Vec<Target>) {
    let connected = connected_with_tracks(
        Agent::new(config()).unwrap(),
        scoped_desired(1),
        "publisher",
        "video-track",
        "audio-track",
    );
    assert_eq!(connected.3.len(), 2);
    (connected.0, connected.1, connected.2, connected.3)
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
    agent.command(AgentCommand::Join("room".into())).unwrap();
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
    let (operation, _) = native_request(&mut agent, generation);
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
) -> (Agent, Generation, ChannelId, Vec<Target>, Vec<Target>) {
    connected_with_tracks(
        agent,
        initial,
        "pa_11111111111111111111111111",
        "video-one",
        "audio-one",
    )
}

fn connected_with_tracks(
    agent: Agent,
    initial: DesiredState,
    participant_id: &str,
    video_track: &str,
    audio_track: &str,
) -> (Agent, Generation, ChannelId, Vec<Target>, Vec<Target>) {
    let (mut agent, generation, channel) = begin(agent, initial, |_| {});
    agent
        .handle(HostEvent::DataChannel(DataChannelEvent::Message {
            generation,
            channel,
            payload: codec::encode_server(&state::ServerStateBatch {
                records: Vec::new(),
                initial_complete: true,
                ..Default::default()
            })
            .unwrap(),
        }))
        .unwrap();
    let pending = next(&mut agent, generation, channel);
    let lookups = pending
        .iter()
        .filter(|target| {
            matches!(
                target.spec().declaration,
                Some(state::resource_spec::Declaration::SourceLookup(_))
            )
        })
        .map(|target| {
            let Some(state::resource_spec::Declaration::SourceLookup(lookup)) =
                &target.spec().declaration
            else {
                panic!("source lookup")
            };
            let address = lookup.source.as_ref().unwrap();
            let participant = state::Participant {
                participant_id: participant_id.into(),
                participant_external_id: address.participant_external_id.clone(),
            };
            let (track_id, kind) = match state::TrackKind::try_from(address.kind).unwrap() {
                state::TrackKind::Video => (
                    video_track,
                    state::track::Kind::Video(state::VideoTrack {
                        format: Some(format(MediaKind::Video)),
                    }),
                ),
                state::TrackKind::Audio => (
                    audio_track,
                    state::track::Kind::Audio(state::AudioTrack {
                        format: Some(format(MediaKind::Audio)),
                    }),
                ),
                _ => panic!("media source lookup"),
            };
            native_connection::confirmed(
                state::resource_key::Key::Owned(target.reference),
                target.desired.revision,
                state::confirmed_state::Value::SourceLookup(Box::new(state::SourceLookupState {
                    spec: Some(target.spec().clone()),
                    participant: Some(participant.clone()),
                    track: Some(state::Track {
                        track_id: track_id.into(),
                        participant_id: participant.participant_id,
                        label: address.label.clone(),
                        kind: Some(kind),
                    }),
                })),
            )
        })
        .collect::<Vec<_>>();
    let lookup_targets = pending
        .iter()
        .filter(|target| {
            matches!(
                target.spec().declaration,
                Some(state::resource_spec::Declaration::SourceLookup(_))
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut targets = pending
        .into_iter()
        .filter(|target| {
            !matches!(
                target.spec().declaration,
                Some(state::resource_spec::Declaration::SourceLookup(_))
            )
        })
        .collect::<Vec<_>>();
    if !lookups.is_empty() {
        native_connection::observe(&mut agent, generation, channel, lookups);
        if let Some(mut resolved) = next_if_ready(&mut agent, generation, channel) {
            targets.append(&mut resolved);
        } else {
            targets.extend(lookup_targets.clone());
        }
    }
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
    assert!(agent.next_effect_after_turn().is_none());
    (agent, generation, channel, targets, lookup_targets)
}
