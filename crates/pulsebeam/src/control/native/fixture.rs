use super::handler::StateProtocolHandler;
use super::handoff::{self, EndpointHandoff};
use super::{InputError, NativeSession};
use crate::entity::{ConnectionId, ParticipantExternalId, ParticipantId, RoomId, TrackId};
use crate::track::Track;

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
        let mut handler =
            StateProtocolHandler::new(participant, room, external, capabilities, controller)
                .unwrap();
        handler
            .model
            .authorization(i64::try_from(expiry).unwrap(), 1)
            .unwrap();
        (Self { handler, ready }, endpoint)
    }

    pub(crate) fn command(&self, command: super::ExecutionCommand) {
        assert!(self.handler.handoff.command(command).is_ok());
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
        self.handler.service(now, source).map(|_| ())
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
