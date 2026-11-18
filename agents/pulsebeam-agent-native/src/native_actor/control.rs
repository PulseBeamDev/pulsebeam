use super::*;

#[derive(Clone)]
pub(super) struct Control {
    core: Arc<std::sync::Mutex<agent_core::Agent>>,
    pub(super) changed: Arc<Notify>,
    observer: Arc<agent_core::observer::AgentStateObserver>,
}

impl Control {
    pub(super) fn new(mut core: agent_core::Agent) -> Self {
        core.defer_logs();
        let observer = Arc::new(agent_core::observer::AgentStateObserver::new(
            core.snapshot().into(),
        ));
        Self {
            observer,
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

    fn update<T>(&self, operation: impl FnOnce(&mut agent_core::Agent) -> T) -> T {
        let (result, version, state) = self.with(|core| {
            let result = operation(core);
            (result, core.snapshot().version, core.snapshot().into())
        });
        // Foreign cursor wake callbacks may reenter this actor. Publish only after
        // releasing its lock, and fence projections overtaken by another caller.
        let _ = self.observer.publish_from(version, state);
        result
    }

    pub(super) fn state(&self) -> agent_core::ffi::AgentState {
        self.update(|_| ());
        self.observer.state()
    }

    pub(super) fn updates(
        &self,
    ) -> Result<agent_core::observer::AgentUpdates, agent_core::observer::ObserverLimit> {
        self.update(|_| ());
        self.observer.updates()
    }

    pub(super) fn close_observation(&self) {
        let mut state = self.state();
        state.room = None;
        let _ = self.observer.close(state);
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
        let result = self.update(operation);
        self.changed.notify_one();
        result
    }

    pub(super) fn command(&self, command: AgentCommand) -> Result<(), agent_core::AgentError> {
        self.update(|core| core.command(command))
    }

    pub(super) fn handle(&self, event: HostEvent) -> Result<(), agent_core::AgentError> {
        self.update(|core| core.handle(event))
    }

    pub(super) fn resource(
        &self,
        command: agent_core::ResourceCommand,
    ) -> Result<agent_core::ResourceResult, agent_core::AgentError> {
        self.update(|core| core.resource(command))
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

    struct Reenter(Control);

    impl std::task::Wake for Reenter {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            assert!(self.0.core.try_lock().is_ok());
            let _ = self.0.state();
        }
    }

    #[tokio::test]
    async fn public_state_observes_synchronous_intent_and_one_final_close() {
        let control = Control::new(
            agent_core::Agent::new(agent_core::AgentConfig {
                endpoint: "http://pulsebeam.test".into(),
                token: "initial".into(),
                retry: Default::default(),
                log_level: agent_core::LogLevel::Off,
            })
            .unwrap(),
        );
        let a = control.updates().unwrap();
        let b = control.updates().unwrap();
        let initial = control.state();
        assert_eq!(a.next().unwrap().await, Some(initial.clone()));
        assert_eq!(b.next().unwrap().await, Some(initial.clone()));
        let mut pending = Box::pin(a.next().unwrap());
        let waker = std::task::Waker::from(Arc::new(Reenter(control.clone())));
        let mut context = std::task::Context::from_waker(&waker);
        assert!(pending.as_mut().poll(&mut context).is_pending());
        control
            .external(|core| core.command(AgentCommand::Join("first".into())))
            .unwrap();
        let joined_intent = control.state();
        assert_eq!(pending.await, Some(joined_intent.clone()));
        assert_eq!(joined_intent.desired_room.as_deref(), Some("first"));
        assert_eq!(joined_intent.room, None);
        assert_eq!(
            joined_intent.desired_join_revision,
            initial.desired_join_revision + 1
        );
        assert_eq!(joined_intent.evaluated_join_revision, 0);
        control
            .external(|core| core.command(AgentCommand::RenewAuthorization("next".into())))
            .unwrap();
        let latest = control.state();
        assert_eq!(
            latest.desired_credential_revision,
            initial.desired_credential_revision + 1
        );
        assert_eq!(
            latest.evaluated_credential_revision,
            initial.evaluated_credential_revision
        );
        assert!(
            control
                .external(|core| core.command(AgentCommand::RenewAuthorization(String::new())))
                .is_err()
        );
        assert_eq!(control.state(), latest);
        assert_eq!(a.next().unwrap().await, Some(latest.clone()));
        assert_eq!(b.next().unwrap().await, Some(latest));
        control.close_observation();
        let closed = control.state();
        assert_eq!(closed.status, agent_core::ffi::AgentStatus::Closed);
        control.close_observation();
        assert_eq!(control.state(), closed);
        assert_eq!(a.next().unwrap().await, Some(closed.clone()));
        assert_eq!(a.next().unwrap().await, None);
        assert_eq!(b.next().unwrap().await, Some(closed));
        assert_eq!(b.next().unwrap().await, None);
    }

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
