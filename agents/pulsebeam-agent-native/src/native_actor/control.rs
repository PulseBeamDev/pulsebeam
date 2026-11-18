use super::*;

#[derive(Clone)]
pub(super) struct Control {
    core: Arc<std::sync::Mutex<agent_core::Agent>>,
    pub(super) changed: Arc<Notify>,
}

impl Control {
    pub(super) fn new(mut core: agent_core::Agent) -> Self {
        core.defer_logs();
        Self {
            core: Arc::new(std::sync::Mutex::new(core)),
            changed: Arc::new(Notify::new()),
        }
    }

    fn with<T>(&self, operation: impl FnOnce(&mut agent_core::Agent) -> T) -> T {
        let mut core = self
            .core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        operation(&mut core)
    }

    pub(super) fn flush_logs(&self) {
        self.flush_with(agent_core::DeferredLog::emit);
    }

    fn flush_with(&self, mut emit: impl FnMut(agent_core::DeferredLog)) {
        let logs = self.with(agent_core::Agent::drain_logs);
        // Logger callbacks can join again. Their diagnostics wait for the next host turn.
        for record in logs {
            emit(record);
        }
    }

    pub(super) fn external<T>(&self, operation: impl FnOnce(&mut agent_core::Agent) -> T) -> T {
        let result = self.with(operation);
        self.changed.notify_one();
        result
    }

    pub(super) fn command(&self, command: AgentCommand) -> Result<(), agent_core::AgentError> {
        self.with(|core| core.command(command))
    }

    pub(super) fn handle(&self, event: HostEvent) -> Result<(), agent_core::AgentError> {
        self.with(|core| core.handle(event))
    }

    pub(super) fn resource(
        &self,
        command: agent_core::ResourceCommand,
    ) -> Result<agent_core::ResourceResult, agent_core::AgentError> {
        self.with(|core| core.resource(command))
    }

    pub(super) fn send_data(
        &self,
        spec: agent_core::TrackSpec,
        data: Vec<u8>,
    ) -> Result<OperationId, agent_core::AgentError> {
        self.with(|core| core.send_data(spec, data))
    }

    pub(super) fn snapshot(&self) -> Snapshot {
        self.with(|core| core.snapshot().clone())
    }

    pub(super) fn next_effect(&self) -> Option<Effect> {
        self.with(agent_core::Agent::next_effect)
    }

    pub(super) fn next_notification(&self) -> Option<Notification> {
        self.with(agent_core::Agent::next_notification)
    }

    pub(super) fn is_publishing(&self, slot: &MediaSlot) -> bool {
        self.with(|core| core.is_publishing(slot))
    }

    pub(super) fn source_readiness(&self, slot: &MediaSlot) -> SourceReadiness {
        self.with(|core| core.source_preparation(slot).readiness)
    }

    pub(super) fn publication_input_control_current(
        &self,
        control: &agent_core::PublicationInputControl,
    ) -> bool {
        self.with(|core| core.publication_input_control_current(control))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logger_delivery_can_reenter_the_same_core_after_commit() {
        let control = Control::new(
            agent_core::Agent::new(agent_core::AgentConfig {
                endpoint: "http://pulsebeam.test".into(),
                token: "token".into(),
                retry: Default::default(),
                log_level: agent_core::LogLevel::Info,
            })
            .unwrap(),
        );
        control
            .external(|core| core.command(AgentCommand::Join("first".into())))
            .unwrap();
        let mut emitted = 0;
        control.flush_with(|_| {
            assert!(control.core.try_lock().is_ok());
            if emitted == 0 {
                assert_eq!(control.snapshot().desired_room.as_deref(), Some("first"));
                control
                    .external(|core| core.command(AgentCommand::Join("newest".into())))
                    .unwrap();
            }
            emitted += 1;
        });
        assert!(emitted > 0);
        assert_eq!(control.snapshot().desired_room.as_deref(), Some("newest"));
        assert!(!control.with(agent_core::Agent::drain_logs).is_empty());
    }
}
