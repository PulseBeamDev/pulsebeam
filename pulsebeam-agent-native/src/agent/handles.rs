use pulsebeam_agent_core::{CoreError, CoreEvent, CoreInput, MonotonicTime, ParticipantId};

use super::mailbox::{self, SendError};

#[derive(Clone, Debug)]
pub enum DriverCommand {
    Input {
        now: MonotonicTime,
        input: CoreInput,
    },
    Shutdown,
}

#[derive(Clone)]
pub struct Agent {
    participant_id: ParticipantId,
    commands: mailbox::Sender<DriverCommand>,
}

impl Agent {
    pub(crate) fn new(
        participant_id: ParticipantId,
        commands: mailbox::Sender<DriverCommand>,
    ) -> Self {
        Self {
            participant_id,
            commands,
        }
    }

    pub fn participant_id(&self) -> &ParticipantId {
        &self.participant_id
    }

    pub async fn handle(
        &self,
        now: MonotonicTime,
        input: CoreInput,
    ) -> Result<(), SendError<DriverCommand>> {
        self.commands
            .send(DriverCommand::Input { now, input })
            .await
    }

    pub async fn start(&self, now: MonotonicTime) -> Result<(), SendError<DriverCommand>> {
        self.handle(now, CoreInput::Start).await
    }

    pub async fn shutdown(&self) -> Result<(), SendError<DriverCommand>> {
        self.commands.send(DriverCommand::Shutdown).await
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AgentEvent {
    Core(CoreEvent),
    EffectExecuted,
    DatagramReceived {
        generation: pulsebeam_agent_core::TransportGeneration,
        bytes: Vec<u8>,
    },
    Error(CoreError),
}
