use std::collections::VecDeque;

use super::handoff::{ControllerHandoff, Fact};
use super::{ExecutionCommand, InputError, NativeSession, OutputError};
use crate::entity::{ParticipantExternalId, ParticipantId, RoomId, TrackId};
use crate::track::Track;

const FACT_BUDGET: usize = 32;
const PREPARATION_BUDGET: usize = 32;
const PENDING_COMMANDS: usize = 64;

pub(in crate::control) struct StateProtocolHandler {
    pub(in crate::control) participant: ParticipantId,
    pub(in crate::control) room: RoomId,
    pub(in crate::control) model: NativeSession,
    pub(in crate::control) handoff: ControllerHandoff,
    external: ParticipantExternalId,
    capabilities: Option<pulsebeam_proto::state::RtpCapabilities>,
    commands: VecDeque<ExecutionCommand>,
}

impl StateProtocolHandler {
    pub(in crate::control) fn new(
        participant: ParticipantId,
        room: RoomId,
        external: ParticipantExternalId,
        capabilities: Option<pulsebeam_proto::state::RtpCapabilities>,
        handoff: ControllerHandoff,
    ) -> Result<Self, OutputError> {
        let mut model = NativeSession::default();
        model.observe_participant(pulsebeam_proto::state::Participant {
            participant_id: participant.as_str(),
            participant_external_id: external.as_str().to_owned(),
        })?;
        Ok(Self {
            participant,
            room,
            external,
            model,
            handoff,
            capabilities,
            commands: VecDeque::new(),
        })
    }

    fn retain(&mut self, command: ExecutionCommand) -> Result<(), OutputError> {
        if self.commands.len() == PENDING_COMMANDS {
            return Err(OutputError::Capacity);
        }
        self.commands.push_back(command);
        Ok(())
    }

    fn flush_commands(&mut self) -> bool {
        let mut transferred = false;
        for _ in 0..PREPARATION_BUDGET {
            let Some(command) = self.commands.pop_front() else {
                break;
            };
            if let Err(command) = self.handoff.command(command) {
                self.commands.push_front(command);
                break;
            }
            transferred = true;
        }
        transferred
    }

    // Returns whether a coalesced endpoint wake is needed. Queue admission is not execution.
    pub(in crate::control) fn service<'a>(
        &mut self,
        unix_now: u64,
        source: impl Fn(TrackId) -> Option<&'a Track>,
    ) -> Result<bool, InputError> {
        if self.handoff.failed() {
            return Err(OutputError::InvalidObservation.into());
        }
        // Timestamped RTC progress must win over a controller-observed timeout.
        if let Some(progress) = self.handoff.progress() {
            self.model.commit_sent(progress)?;
        }
        if let Some(opened) = self.handoff.opened() {
            self.model.set_cid(opened.channel, opened.at);
        }
        let mut wake = self.flush_commands();
        for _ in 0..FACT_BUDGET {
            let Some(fact) = self.handoff.fact() else {
                break;
            };
            match fact {
                Fact::Completion(completion) => {
                    self.model.observe_completion(completion, &self.external)?;
                }
                Fact::Deferral(deferral) => self.model.observe_deferral(deferral)?,
                Fact::Resource(observation) => {
                    if let Some(command) = self.model.observe_resource(observation)? {
                        self.retain(command)?;
                    }
                }
                Fact::ReconstructionRequired => {
                    self.model
                        .close(pulsebeam_proto::state::CloseReason::ReconstructionRequired)?;
                }
            }
        }
        // One bounded wire batch per quantum, without making essential facts wait for input space.
        if self.commands.len() < PREPARATION_BUDGET
            && let Some(bytes) = self.handoff.input()
        {
            for command in self.model.receive_input(&bytes, unix_now)? {
                self.retain(command)?;
            }
        }
        self.model.service_source_waits(
            unix_now,
            pulsebeam_runtime::time::Instant::now(),
            |track| source(track).is_some(),
        )?;
        for _ in 0..PREPARATION_BUDGET {
            if self.commands.len() >= PREPARATION_BUDGET
                || !self.model.preparation_pending(unix_now)
            {
                break;
            }
            let Some(work) = self.model.take_preparation()? else {
                break;
            };
            if let Some(command) = self.model.prepare_execution(
                &work,
                unix_now,
                self.capabilities.as_ref(),
                &source,
            )? {
                self.retain(command)?;
            }
        }
        wake |= self.flush_commands();
        self.model.poll()?;
        if let Some(output) = self.model.take_output() {
            match self.handoff.output(output) {
                Ok(()) => wake = true,
                Err(output) => self.model.restore_output(output)?,
            }
        }
        if !self
            .handoff
            .rearm_for(self.commands.len() < PREPARATION_BUDGET)
        {
            return Err(OutputError::Capacity.into());
        }
        if self.commands.len() < PREPARATION_BUDGET
            && self.model.preparation_pending(unix_now)
            && !self.handoff.schedule()
        {
            return Err(OutputError::Capacity.into());
        }
        Ok(wake && self.handoff.wake_endpoint())
    }
}

impl Drop for StateProtocolHandler {
    fn drop(&mut self) {
        self.handoff.stop();
    }
}
