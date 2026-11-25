use super::handler::StateProtocolHandler;
use super::handoff::{self, EndpointHandoff};
use super::{InputError, NativeSession, OutputError, ResolvedSource};
use crate::entity::{ConnectionId, ParticipantId, PublicationKey, RoomId};
use crate::track::{Track, TrackMeta};
use pulsebeam_proto::state as wire;
use std::collections::BTreeMap;

fn opus() -> wire::CodecFormat {
    crate::control::rtp::capabilities().codecs[0]
        .format
        .clone()
        .unwrap()
}

pub(crate) fn audio_track(meta: TrackMeta) -> Track {
    let Track::Audio(mut audio) = Track::audio(meta, None) else {
        unreachable!()
    };
    audio.format = Some(opus());
    Track::Audio(audio)
}

pub(crate) fn audio_receiver(publication: PublicationKey, room: &str) -> wire::ResourceSpec {
    wire::ResourceSpec {
        settings: Some(wire::ResourceSettings {
            enabled: Some(true),
            receive: Some(wire::resource_settings::Receive::Audio(
                wire::AudioReceiveOptions::default(),
            )),
        }),
        declaration: Some(wire::resource_spec::Declaration::Subscription(
            wire::SubscriptionSpec {
                publication_id: publication.as_str(),
                kind: wire::TrackKind::Audio as i32,
                source_room: room.into(),
                binding: Some(audio_receiver_binding()),
            },
        )),
    }
}

fn audio_receiver_binding() -> wire::TrackBinding {
    wire::TrackBinding {
        transport: Some(wire::track_binding::Transport::Rtp(wire::RtpParameters {
            extmap_allow_mixed: None,
            mid: "audio".into(),
            codec: Some(wire::RtpCodecParameters {
                payload_type: Some(111),
                rtx_payload_type: None,
                feedback: vec![wire::RtcpFeedback::TransportCc.as_str().into()],
                format: Some(opus()),
            }),
            header_extensions: [
                wire::RtpHeaderExtensionType::RtpHeaderExtensionMid,
                wire::RtpHeaderExtensionType::RtpHeaderExtensionTransportCcDraft01,
            ]
            .into_iter()
            .enumerate()
            .map(|(index, uri)| wire::RtpHeaderExtensionParameters {
                uri: uri.as_str().into(),
                id: u32::try_from(index).unwrap() + 1,
            })
            .collect(),
            encodings: vec![wire::RtpEncodingParameters {
                ssrc: Some(7),
                ..Default::default()
            }],
            rtcp: Some(wire::RtcpParameters {
                cname: "receiver".into(),
                reduced_size: false,
            }),
        })),
    }
}

pub(crate) fn data_v1_binding(stream: u32) -> wire::TrackBinding {
    wire::TrackBinding {
        transport: Some(wire::track_binding::Transport::SctpDataV1(
            wire::SctpDataV1Parameters {
                stream_id: Some(stream),
                version: u32::from(pulsebeam_proto::data_endpoint::DATA_V1),
                channel_incarnation: u64::from(stream),
                delivery: wire::DataV1Delivery::Latest as i32,
            },
        )),
    }
}

pub(crate) fn execution_handoff(
    resources: &mut crate::control::NegotiatedResources,
    connection: ConnectionId,
) -> (
    handoff::ControllerHandoff,
    pulsebeam_queue::Receiver<ConnectionId>,
) {
    let (sender, receiver) = pulsebeam_queue::bounded(std::num::NonZeroUsize::MIN);
    let (controller, endpoint) = handoff::pair(connection, sender);
    resources.native_handoff = Some(endpoint);
    (controller, receiver)
}

pub(crate) struct NativeControl {
    handler: StateProtocolHandler,
    ready: pulsebeam_queue::Receiver<ConnectionId>,
    publishers: BTreeMap<ParticipantId, ParticipantId>,
    data_branches: super::DataBranchIds,
}

impl NativeControl {
    pub(crate) fn new(
        participant: ParticipantId,
        connection: ConnectionId,
        room: RoomId,
        capabilities: Option<pulsebeam_proto::state::RtpCapabilities>,
        expiry: u64,
    ) -> (Self, EndpointHandoff) {
        let (sender, ready) = pulsebeam_queue::bounded(std::num::NonZeroUsize::MIN);
        let (controller, endpoint) = handoff::pair(connection, sender);
        let publishers = BTreeMap::from([(participant, participant)]);
        let mut handler =
            StateProtocolHandler::new(participant, Some(room), capabilities, controller).unwrap();
        handler
            .model
            .authorization(
                i64::try_from(expiry).unwrap(),
                1,
                super::ResourcePermissions::RoomBound,
            )
            .unwrap();
        (
            Self {
                handler,
                ready,
                publishers,
                data_branches: super::DataBranchIds::default(),
            },
            endpoint,
        )
    }

    pub(crate) fn observe_participant(
        &mut self,
        participant: pulsebeam_proto::state::Participant,
    ) -> Result<(), OutputError> {
        let id = participant
            .participant_id
            .parse()
            .map_err(|_| OutputError::InvalidObservation)?;
        let external = ParticipantId::new(&participant.participant_id)
            .map_err(|_| OutputError::InvalidObservation)?;
        self.handler.model.observe_participant(participant)?;
        self.publishers.insert(id, external);
        Ok(())
    }

    pub(crate) fn observe_candidate(&mut self, track: &Track) -> Result<(), OutputError> {
        let publisher = self
            .publishers
            .get(&track.meta().origin)
            .ok_or(OutputError::InvalidObservation)?;
        self.handler
            .model
            .observe_candidate(ResolvedSource { track, publisher })
    }

    pub(crate) fn command(&self, command: super::ExecutionCommand) {
        assert!(self.handler.handoff.command(command).is_ok());
    }

    pub(crate) fn begin_terminal(
        &mut self,
        reason: pulsebeam_proto::state::CloseReason,
    ) -> Result<(), InputError> {
        self.handler.close(reason)?;
        self.handler.service_terminal().map(|_| ())
    }

    pub(crate) fn service<'a>(
        &mut self,
        source: impl Fn(PublicationKey) -> Option<&'a Track>,
    ) -> Result<(), InputError> {
        self.service_with_lookup(source, |_| None)
    }

    pub(crate) fn service_with_lookup<'a>(
        &mut self,
        source: impl Fn(PublicationKey) -> Option<&'a Track>,
        lookup: impl Fn(&pulsebeam_proto::state::LogicalSourceAddress) -> Option<&'a Track>,
    ) -> Result<(), InputError> {
        while self.ready.try_recv().is_ok() {}
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let publishers = &self.publishers;
        self.handler
            .service(
                now,
                &self.data_branches,
                |id| {
                    let track = source(id)?;
                    Some(ResolvedSource {
                        track,
                        publisher: publishers.get(&track.meta().origin)?,
                    })
                },
                |_, id| {
                    let track = source(id)?;
                    Some(ResolvedSource {
                        track,
                        publisher: publishers.get(&track.meta().origin)?,
                    })
                },
                |address| {
                    let track = lookup(address)?;
                    Some(ResolvedSource {
                        track,
                        publisher: &track.meta().origin,
                    })
                },
                |_, _, _| None,
            )
            .map(|_| ())
    }
}

impl std::ops::Deref for NativeControl {
    type Target = NativeSession;

    fn deref(&self) -> &Self::Target {
        &self.handler.model
    }
}

impl std::ops::DerefMut for NativeControl {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.handler.model
    }
}
