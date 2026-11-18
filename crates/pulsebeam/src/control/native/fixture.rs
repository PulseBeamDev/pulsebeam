use super::handler::StateProtocolHandler;
use super::handoff::{self, EndpointHandoff};
use super::{InputError, NativeSession, OutputError, ResolvedSource};
use crate::entity::{ConnectionId, ParticipantExternalId, ParticipantId, RoomId, TrackId};
use crate::track::Track;
use std::collections::BTreeMap;

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
    publishers: BTreeMap<ParticipantId, ParticipantExternalId>,
}

impl NativeControl {
    pub(crate) fn new(
        participant: ParticipantId,
        connection: ConnectionId,
        room: RoomId,
        external: ParticipantExternalId,
        capabilities: Option<pulsebeam_proto::state::RtpCapabilities>,
        expiry: u64,
    ) -> (Self, EndpointHandoff) {
        let (sender, ready) = pulsebeam_queue::bounded(std::num::NonZeroUsize::MIN);
        let (controller, endpoint) = handoff::pair(connection, sender);
        let publishers = BTreeMap::from([(participant, external.clone())]);
        let mut handler =
            StateProtocolHandler::new(participant, room, external, capabilities, controller)
                .unwrap();
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
        let external = ParticipantExternalId::new(&participant.participant_external_id)
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
        source: impl Fn(TrackId) -> Option<&'a Track>,
    ) -> Result<(), InputError> {
        while self.ready.try_recv().is_ok() {}
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let publishers = &self.publishers;
        self.handler
            .service(now, |id| {
                let track = source(id)?;
                Some(ResolvedSource {
                    track,
                    publisher: publishers.get(&track.meta().origin)?,
                })
            })
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
