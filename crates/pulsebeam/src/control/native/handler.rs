use std::collections::VecDeque;

use super::handoff::{ControllerHandoff, Fact};
use super::{ExecutionCommand, InputError, NativeSession, OutputError, ResolvedSource};
use crate::entity::{ParticipantId, PublicationKey, RoomId};

const FACT_BUDGET: usize = 32;
const PREPARATION_BUDGET: usize = 32;
const PENDING_COMMANDS: usize = 64;

pub(in crate::control) struct StateProtocolHandler {
    pub(in crate::control) participant: ParticipantId,
    pub(in crate::control) room: Option<RoomId>,
    pub(in crate::control) model: NativeSession,
    pub(in crate::control) handoff: ControllerHandoff,
    pub(in crate::control) successor_refresh: Option<pulsebeam_runtime::time::Instant>,
    capabilities: Option<pulsebeam_proto::state::RtpCapabilities>,
    commands: VecDeque<ExecutionCommand>,
}

impl StateProtocolHandler {
    pub(in crate::control) fn new(
        participant: ParticipantId,
        room: Option<RoomId>,
        capabilities: Option<pulsebeam_proto::state::RtpCapabilities>,
        handoff: ControllerHandoff,
    ) -> Result<Self, OutputError> {
        let mut model = NativeSession::default();
        if let Some(room) = room {
            model.observe_self(pulsebeam_proto::state::Participant {
                participant_id: participant.as_str().to_owned(),
                room_id: room.as_str().to_owned(),
            })?;
        }
        Ok(Self {
            participant,
            room,
            model,
            handoff,
            successor_refresh: None,
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

    pub(in crate::control) fn close(
        &mut self,
        reason: pulsebeam_proto::state::CloseReason,
    ) -> Result<(), OutputError> {
        self.handoff.stop_execution();
        self.model.close(reason)
    }

    pub(in crate::control) fn service_terminal(&mut self) -> Result<bool, InputError> {
        if self.handoff.failed() {
            return Err(OutputError::InvalidObservation.into());
        }
        self.commands.clear();
        self.model.finish_closed_lookups(FACT_BUDGET)?;
        if let Some(progress) = self.handoff.progress() {
            self.model.commit_sent(progress)?;
        }
        if let Some(opened) = self.handoff.opened() {
            self.model.set_cid(opened.channel, opened.at);
        }
        // A displaced endpoint may still report old execution and client input.
        // Drain those bounded lanes without applying any more resource mutations.
        for _ in 0..FACT_BUDGET {
            if self.handoff.fact().is_none() {
                break;
            }
        }
        let _ = self.handoff.input();
        if self.model.terminal_sent() {
            if !self.handoff.rearm_for(false) {
                return Err(OutputError::Capacity.into());
            }
            return Ok(false);
        }
        self.model.poll()?;
        let mut wake = false;
        if let Some(output) = self.model.take_output() {
            match self.handoff.output(output) {
                Ok(()) => wake = true,
                Err(output) => self.model.restore_output(output)?,
            }
        }
        if !self.handoff.rearm_for(false) {
            return Err(OutputError::Capacity.into());
        }
        Ok(wake && self.handoff.wake_endpoint())
    }

    // Returns whether a coalesced endpoint wake is needed. Queue admission is not execution.
    pub(in crate::control) fn service<'a>(
        &mut self,
        unix_now: u64,
        source: impl Fn(PublicationKey) -> Option<ResolvedSource<'a>>,
        source_in_room: impl Fn(&str, PublicationKey) -> Option<ResolvedSource<'a>>,
        lookup_source: impl Fn(
            &pulsebeam_proto::state::LogicalSourceAddress,
        ) -> Option<ResolvedSource<'a>>,
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
                    self.model
                        .observe_completion(completion, &self.participant)?;
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
        for command in self.model.service_revocations(
            PREPARATION_BUDGET.saturating_sub(self.commands.len()),
            &source,
        )? {
            self.retain(command)?;
        }
        // One bounded wire batch per quantum, without making essential facts wait for input space.
        if self.commands.len() < PREPARATION_BUDGET
            && let Some(bytes) = self.handoff.input()
        {
            for command in self.model.receive_input(&bytes, unix_now)? {
                self.retain(command)?;
            }
        }
        self.model
            .refresh_source_lookups(FACT_BUDGET, unix_now, &lookup_source)?;
        self.model.service_source_waits_in_room(
            unix_now,
            pulsebeam_runtime::time::Instant::now(),
            |room, track| {
                source_in_room(room, track)
                    .is_some_and(|source| source.track.id() == track && source.is_available())
            },
        )?;
        while self.commands.len() < PREPARATION_BUDGET {
            let Some(command) = self.model.take_logical_retirement()? else {
                break;
            };
            self.retain(command)?;
        }
        for _ in 0..PREPARATION_BUDGET {
            if self.commands.len() >= PREPARATION_BUDGET
                || !self.model.preparation_pending(unix_now)
            {
                break;
            }
            let Some(work) = self.model.take_preparation()? else {
                break;
            };
            if let Some(command) = self.model.prepare_execution_in_room(
                &work,
                unix_now,
                self.capabilities.as_ref(),
                &source_in_room,
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
            && (self.model.revocation_pending()
                || self.model.preparation_pending(unix_now)
                || self.model.lookup_refresh_pending()
                || self.model.logical_retirement_pending()
                || self.model.catalog.pending() && self.model.needs_poll())
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
