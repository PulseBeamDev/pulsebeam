use super::*;
use crate::{client_sdp, rtc_preparation};
use pulsebeam_proto::state::{self, codec};

#[derive(Clone)]
pub(super) struct Target {
    pub reference: u64,
    pub desired: state::DesiredState,
    pub prepared: Option<state::TrackBinding>,
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
        let track = tracks
            .iter()
            .find(|track| match self.spec().declaration.as_ref().unwrap() {
                state::resource_spec::Declaration::Subscription(subscription) => {
                    track.publication_id == subscription.publication_id
                }
                state::resource_spec::Declaration::LogicalSubscription(subscription) => {
                    state::validation::matches_source_address(
                        track,
                        subscription.source.as_ref().unwrap(),
                    )
                }
                _ => false,
            })
            .unwrap();
        self.installed(participant.clone(), track.clone())
    }

    pub fn offer(
        &self,
        revision: u64,
        source: Option<state::Track>,
        execution: Option<state::logical_subscription_state::Execution>,
    ) -> state::ServerStateRecord {
        native_connection::confirmed(
            state::resource_key::Key::Owned(self.reference),
            self.desired.revision,
            state::confirmed_state::Value::LogicalSubscription(Box::new(
                state::LogicalSubscriptionState {
                    data_v1: None,
                    spec: Some(self.spec().clone()),
                    source_revision: revision,
                    source,
                    execution,
                },
            )),
        )
    }

    pub fn installed_video_publication(
        &self,
        participant: state::Participant,
        publication_id: &str,
    ) -> state::ServerStateRecord {
        let Some(state::resource_spec::Declaration::Publication(publication)) =
            self.spec().declaration.as_ref()
        else {
            panic!("publication declaration");
        };
        let Some(state::publication_spec::Kind::Video(video)) = publication.kind.as_ref() else {
            panic!("video publication");
        };
        let Some(state::track_binding::Transport::Rtp(rtp)) = video
            .binding
            .as_ref()
            .and_then(|binding| binding.transport.as_ref())
        else {
            panic!("prepared RTP sender");
        };
        let track = state::Track {
            publication_id: publication_id.into(),
            participant_id: participant.participant_id.clone(),
            room_id: participant.room_id.clone(),
            track_id: publication.track_id.clone(),
            kind: Some(state::track::Kind::Video(state::VideoTrack {
                av1_whole_source: false,
                guarantees_video_dd: false,
                format: rtp.codec.as_ref().and_then(|codec| codec.format.clone()),
            })),
        };
        self.installed(participant, track)
    }

    pub fn installed(
        &self,
        participant: state::Participant,
        track: state::Track,
    ) -> state::ServerStateRecord {
        if matches!(
            self.spec().declaration,
            Some(state::resource_spec::Declaration::LogicalSubscription(_))
        ) {
            return self.offer(
                1,
                Some(track),
                Some(state::logical_subscription_state::Execution::Installed(
                    self.prepared.clone().expect("prepared logical receiver"),
                )),
            );
        }
        native_connection::confirmed(
            state::resource_key::Key::Owned(self.reference),
            self.desired.revision,
            state::confirmed_state::Value::Owned(alloc::boxed::Box::new(state::LiveResource {
                data_v1_source: None,
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

pub(super) fn track(id: &str, participant: &str, track_id: &str, kind: MediaKind) -> state::Track {
    let format = Some(format(kind));
    state::Track {
        publication_id: id.into(),
        participant_id: participant.into(),
        room_id: "room".into(),
        track_id: track_id.into(),
        kind: Some(match kind {
            MediaKind::Video => state::track::Kind::Video(state::VideoTrack {
                format,
                ..Default::default()
            }),
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

        room_id: "room".into(),
    };
    let tracks = [
        track("video-track", "publisher", "camera", MediaKind::Video),
        track("audio-track", "publisher", "microphone", MediaKind::Audio),
    ];
    let mut records = vec![native_connection::confirmed(
        state::resource_key::Key::Participant(participant.clone()),
        0,
        state::confirmed_state::Value::Participant(participant.clone()),
    )];
    records.extend(tracks.iter().map(|track| {
        native_connection::confirmed(
            state::resource_key::Key::Publication(track.publication_id.clone()),
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
    Some(
        next_state_records(agent, generation, channel)?
            .into_iter()
            .map(|record| Target {
                reference: record.owned,
                desired: record.desired.expect("desired resource"),
                prepared: None,
            })
            .collect(),
    )
}

pub(super) fn next_reports(
    agent: &mut Agent,
    generation: Generation,
    channel: ChannelId,
) -> Vec<(u64, state::LogicalReceiverReport)> {
    next_state_records(agent, generation, channel)
        .expect("receiver reports")
        .into_iter()
        .map(|record| {
            assert!(record.desired.is_none());
            let Some(state::execution_report::Outcome::LogicalReceiver(report)) =
                record.execution.expect("receiver execution report").outcome
            else {
                panic!("logical receiver report");
            };
            (record.owned, report)
        })
        .collect()
}

fn next_state_records(
    agent: &mut Agent,
    generation: Generation,
    channel: ChannelId,
) -> Option<Vec<state::ClientStateRecord>> {
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
    Some(batch.records)
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
                        Some(state::resource_spec::Declaration::LogicalSubscription(_))
                    ) =>
                {
                    return target.offer(0, None, None);
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
            video_codec: (assigned.kind == MediaKind::Video
                && assigned.direction == MediaDirection::SendOnly)
                .then_some(VideoCodecRequirement::H264),
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
        let format = if assigned.kind == MediaKind::Video
            && assigned.direction == MediaDirection::SendOnly
        {
            state::CodecFormat::from(state::codec_format::Codec::H264(Default::default()))
        } else {
            format(assigned.kind)
        };
        let (mut binding, capability) = rtc_preparation::binding_fixture(format);
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
    state.video[0].publication_id.clear();
    state.video[0].selector = Some(TrackSelector {
        room: "room".into(),
        participant_id: "publisher".into(),
        track_id: "camera".into(),
    });
    state.audio.pinned.clear();
    state.audio.selected = vec![TrackSelector {
        room: "room".into(),
        participant_id: "publisher".into(),
        track_id: "microphone".into(),
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
    connected_with_tracks(agent, initial, "remote-user", "video-one", "audio-one")
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
            binary: true,
            payload: codec::encode_server(&state::ServerStateBatch {
                records: Vec::new(),
                initial_complete: true,
                ..Default::default()
            })
            .unwrap(),
        }))
        .unwrap();
    let mut pending = next(&mut agent, generation, channel);
    let publication = pending.remove(
        pending
            .iter()
            .position(|target| {
                matches!(
                    target.spec().declaration,
                    Some(state::resource_spec::Declaration::Publication(_))
                )
            })
            .unwrap(),
    );
    let mut targets = pending;
    let offers = targets
        .iter()
        .map(|target| {
            let Some(state::resource_spec::Declaration::LogicalSubscription(subscription)) =
                &target.spec().declaration
            else {
                panic!("logical receive intent");
            };
            let address = subscription.source.as_ref().unwrap();
            let (publication_id, kind) = match state::TrackKind::try_from(address.kind).unwrap() {
                state::TrackKind::Video => (video_track, MediaKind::Video),
                state::TrackKind::Audio => (audio_track, MediaKind::Audio),
                _ => panic!("RTP receiver"),
            };
            let mut source = track(
                publication_id,
                &address.participant_id,
                &address.track_id,
                kind,
            );
            source.room_id = address.room.clone();
            target.offer(1, Some(source), None)
        })
        .collect::<Vec<_>>();
    if !offers.is_empty() {
        native_connection::observe(&mut agent, generation, channel, offers);
        for (reference, report) in next_reports(&mut agent, generation, channel) {
            assert_eq!(report.source_revision, 1);
            let Some(state::logical_receiver_report::Outcome::Prepared(binding)) = report.outcome
            else {
                panic!("prepared RTP fixture");
            };
            targets
                .iter_mut()
                .find(|target| target.reference == reference)
                .unwrap()
                .prepared = Some(binding);
        }
    }
    let lookup_targets = targets.clone();
    let participant = state::Participant {
        participant_id: agent.snapshot().participant_id.clone().unwrap(),

        room_id: agent.snapshot().room_id.clone().unwrap(),
    };
    native_connection::observe(
        &mut agent,
        generation,
        channel,
        vec![publication.installed_video_publication(participant, "local-camera-source")],
    );
    assert_eq!(agent.snapshot().connection, ConnectionState::Connected);
    assert!(agent.next_effect_after_turn().is_none());
    (agent, generation, channel, targets, lookup_targets)
}
