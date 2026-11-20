use super::*;
use std::collections::BTreeMap;

mod catalog;
mod resource_state;

pub(super) enum SubscriptionConfiguration {
    Video(agent_core::VideoObservation),
    Audio(agent_core::PlayoutDelay),
}

type PipeObservers = Arc<
    std::sync::Mutex<
        Vec<(
            agent_core::PublicationInputId,
            std::sync::Weak<agent_core::observer::PipeStateObserver>,
        )>,
    >,
>;

#[derive(Clone)]
pub(super) struct PublicationControl {
    core: std::sync::Weak<std::sync::Mutex<agent_core::Agent>>,
    changed: Arc<Notify>,
    pipe_observers: PipeObservers,
    publication_observers: Arc<
        std::sync::Mutex<
            BTreeMap<u64, std::sync::Weak<agent_core::observer::PublicationStateObserver>>,
        >,
    >,
    agent_observer: Arc<agent_core::observer::AgentStateObserver>,
}

impl PublicationControl {
    pub(super) fn configure(
        &self,
        owner: u64,
        observer: &agent_core::observer::PublicationStateObserver,
        enabled: bool,
    ) -> Result<(), Error> {
        let core = self.core.upgrade().ok_or(Error::Closed)?;
        let mut core = core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        core.resource(agent_core::ResourceCommand::SetPublicationMuted {
            owner,
            muted: !enabled,
        })?;
        let notification = core
            .publication_state(owner)
            .ok()
            .and_then(|state| observer.prepare(state, false).ok());
        drop(core);
        if let Some(notification) = notification {
            let _ = notification.notify();
        }
        self.changed.notify_one();
        Ok(())
    }

    pub(super) fn attach_pipe(
        &self,
        owner: u64,
        publication_closed: &Signal,
    ) -> Result<
        (
            agent_core::PublicationInputOperation,
            Arc<agent_core::observer::PipeStateObserver>,
        ),
        Error,
    > {
        let core = self.core.upgrade().ok_or(Error::Closed)?;
        let mut core = core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if publication_closed.is_closed() {
            return Err(Error::Closed);
        }
        let result =
            core.resource(agent_core::ResourceCommand::AttachPublicationInput { owner })?;
        let agent_core::ResourceResult::PublicationInput { operation, .. } = result else {
            return Err(Error::Rtc("Core did not claim pipe input".into()));
        };
        let input = operation
            .input()
            .ok_or_else(|| Error::Rtc("Core pipe claim omitted input identity".into()))?;
        let state = core
            .pipe_state(input)
            .ok_or_else(|| Error::Rtc("Core pipe state missing after claim".into()))?;
        let observer = Arc::new(agent_core::observer::PipeStateObserver::new(state));
        let mut pipes = self
            .pipe_observers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pipes.retain(|(id, weak)| weak.strong_count() > 0 && core.pipe_state(*id).is_some());
        pipes.push((input, Arc::downgrade(&observer)));
        drop(pipes);
        drop(core);
        Ok((operation, observer))
    }

    pub(super) fn notify_pipe_attached(&self, owner: u64) {
        let Some(core) = self.core.upgrade() else {
            return;
        };
        let core = core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let publication = self
            .publication_observers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&owner)
            .and_then(std::sync::Weak::upgrade)
            .and_then(|observer| {
                core.publication_state(owner)
                    .ok()
                    .and_then(|state| observer.prepare(state, false).ok())
            });
        let snapshot = core.snapshot();
        let version = snapshot.version;
        let agent_state = snapshot.into();
        drop(core);
        if let Some(notification) = publication {
            let _ = notification.notify();
        }
        let _ = self.agent_observer.publish_from(version, agent_state);
        self.changed.notify_one();
    }

    pub(super) fn detach_pipe(
        &self,
        operation: &agent_core::PublicationInputOperation,
        observer: &agent_core::observer::PipeStateObserver,
    ) -> Result<(), Error> {
        let Some(input) = operation.input() else {
            return Ok(());
        };
        let core_arc = self.core.upgrade().ok_or(Error::Closed)?;
        let mut core = core_arc
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let final_state = core.pipe_state(input).unwrap_or_else(|| observer.state());
        match core.resource(agent_core::ResourceCommand::DetachPublicationInput { input })? {
            agent_core::ResourceResult::PublicationInput { operation, .. } => {
                core.resource(agent_core::ResourceCommand::CompletePublicationInput {
                    operation,
                    result: Ok(()),
                })?;
            }
            agent_core::ResourceResult::Superseded => {}
            _ => return Err(Error::Rtc("Core did not detach pipe input".into())),
        }
        let notification = observer
            .prepare(final_state, true)
            .map_err(|_| Error::Backpressure)?;
        let owner = operation.owner();
        let publication = self
            .publication_observers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&owner)
            .and_then(std::sync::Weak::upgrade)
            .and_then(|observer| {
                core.publication_state(owner)
                    .ok()
                    .and_then(|state| observer.prepare(state, false).ok())
            });
        let snapshot = core.snapshot();
        let version = snapshot.version;
        let agent_state = snapshot.into();
        self.pipe_observers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|(id, _)| *id != input);
        drop(core);
        let _ = notification.notify();
        if let Some(notification) = publication {
            let _ = notification.notify();
        }
        let _ = self.agent_observer.publish_from(version, agent_state);
        self.changed.notify_one();
        Ok(())
    }

    pub(super) fn pipe_state(
        &self,
        input: agent_core::PublicationInputId,
        observer: &agent_core::observer::PipeStateObserver,
    ) -> agent_core::ffi::PipeState {
        let Some(core) = self.core.upgrade() else {
            return observer.state();
        };
        let core = core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let notification = core
            .pipe_state(input)
            .and_then(|state| observer.prepare(state, false).ok());
        drop(core);
        if let Some(notification) = notification {
            let _ = notification.notify();
        }
        observer.state()
    }

    pub(super) fn state(
        &self,
        owner: u64,
        observer: &agent_core::observer::PublicationStateObserver,
    ) -> agent_core::ffi::PublicationState {
        let Some(core) = self.core.upgrade() else {
            return observer.state();
        };
        let core = core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let notification = core
            .publication_state(owner)
            .ok()
            .and_then(|state| observer.prepare(state, false).ok());
        drop(core);
        if let Some(notification) = notification {
            let _ = notification.notify();
        }
        observer.state()
    }
}

#[derive(Clone)]
pub(super) struct Control {
    core: Arc<std::sync::Mutex<agent_core::Agent>>,
    pub(super) changed: Arc<Notify>,
    observer: Arc<agent_core::observer::AgentStateObserver>,
    publication_observers: Arc<
        std::sync::Mutex<
            BTreeMap<u64, std::sync::Weak<agent_core::observer::PublicationStateObserver>>,
        >,
    >,
    pipe_observers: PipeObservers,
    resource_observers: Arc<std::sync::Mutex<Vec<resource_state::ResourceObservation>>>,
    watch_observers: catalog::WatchObservers,
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
            publication_observers: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
            pipe_observers: Arc::new(std::sync::Mutex::new(Vec::new())),
            resource_observers: Arc::new(std::sync::Mutex::new(Vec::new())),
            watch_observers: Arc::new(std::sync::Mutex::new(BTreeMap::new())),
        }
    }

    pub(super) fn declare_publication(
        &self,
        spec: agent_core::TrackSpec,
        enabled: bool,
    ) -> Result<(u64, Arc<agent_core::observer::PublicationStateObserver>), Error> {
        let owner = self.update(|core| {
            let agent_core::ResourceResult::Publication { owner, .. } = core
                .resource(agent_core::ResourceCommand::ClaimPublication { spec: spec.clone() })?
            else {
                return Err(agent_core::AgentError::InvalidResource(
                    agent_core::TrackResourceError::WrongKind,
                ));
            };
            if let Err(error) = core.resource(agent_core::ResourceCommand::SetPublicationMuted {
                owner,
                muted: !enabled,
            }) {
                let _ = core.resource(agent_core::ResourceCommand::Release { owner });
                return Err(error);
            }
            Ok::<_, agent_core::AgentError>(owner)
        })?;
        match self.publication_observer(owner) {
            Ok(observer) => Ok((owner, observer)),
            Err(error) => {
                let _ = self.resource(agent_core::ResourceCommand::Release { owner });
                Err(error.into())
            }
        }
    }

    pub(super) fn revoke_publication(&self, owner: u64) {
        let _ = self.resource(agent_core::ResourceCommand::Release { owner });
    }

    pub(super) fn claim_subscription(
        &self,
        selector: agent_core::TrackSelector,
        configuration: SubscriptionConfiguration,
    ) -> Result<u64, Error> {
        self.update(|core| {
            let command: Result<u64, agent_core::AgentError> = match configuration {
                SubscriptionConfiguration::Video(observation) => {
                    let agent_core::ResourceResult::Owned { owner } =
                        core.resource(agent_core::ResourceCommand::ClaimVideo { selector })?
                    else {
                        return Err(agent_core::AgentError::InvalidResource(
                            agent_core::TrackResourceError::WrongKind,
                        ));
                    };
                    if let Err(error) = core
                        .resource(agent_core::ResourceCommand::ObserveVideo { owner, observation })
                    {
                        let _ = core.resource(agent_core::ResourceCommand::Release { owner });
                        return Err(error);
                    }
                    Ok(owner)
                }
                SubscriptionConfiguration::Audio(playout_delay) => {
                    let agent_core::ResourceResult::Owned { owner } =
                        core.resource(agent_core::ResourceCommand::ClaimAudioSubscription {
                            selector,
                        })?
                    else {
                        return Err(agent_core::AgentError::InvalidResource(
                            agent_core::TrackResourceError::WrongKind,
                        ));
                    };
                    if let Err(error) = core.resource(agent_core::ResourceCommand::ConfigureAudio {
                        owner,
                        playout_delay,
                    }) {
                        let _ = core.resource(agent_core::ResourceCommand::Release { owner });
                        return Err(error);
                    }
                    Ok(owner)
                }
            };
            command
        })
        .map_err(Into::into)
    }

    pub(super) fn publication_control(&self) -> PublicationControl {
        PublicationControl {
            core: Arc::downgrade(&self.core),
            changed: self.changed.clone(),
            pipe_observers: self.pipe_observers.clone(),
            publication_observers: self.publication_observers.clone(),
            agent_observer: self.observer.clone(),
        }
    }

    fn publication_projection(
        &self,
        owner: u64,
    ) -> Result<agent_core::ffi::PublicationState, agent_core::AgentError> {
        self.with(|core| core.publication_state(owner))
    }

    pub(super) fn publication_state(
        &self,
        owner: u64,
        observer: &agent_core::observer::PublicationStateObserver,
    ) -> agent_core::ffi::PublicationState {
        let core = self
            .core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let notification = core
            .publication_state(owner)
            .ok()
            .and_then(|state| observer.prepare(state, false).ok());
        drop(core);
        if let Some(notification) = notification {
            let _ = notification.notify();
        }
        observer.state()
    }

    pub(super) fn publication_observer(
        &self,
        owner: u64,
    ) -> Result<Arc<agent_core::observer::PublicationStateObserver>, agent_core::AgentError> {
        if let Some(observer) = self
            .publication_observers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&owner)
            .and_then(std::sync::Weak::upgrade)
        {
            return Ok(observer);
        }
        let initial = self.publication_projection(owner)?;
        let mut observers = self
            .publication_observers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(observer) = observers.get(&owner).and_then(std::sync::Weak::upgrade) {
            return Ok(observer);
        }
        let observer = Arc::new(agent_core::observer::PublicationStateObserver::new(initial));
        observers.insert(owner, Arc::downgrade(&observer));
        drop(observers);
        self.update(|_| ());
        Ok(observer)
    }

    fn with<T>(&self, operation: impl FnOnce(&mut agent_core::Agent) -> T) -> T {
        let mut core = self
            .core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        operation(&mut core)
    }

    fn update<T>(&self, operation: impl FnOnce(&mut agent_core::Agent) -> T) -> T {
        let mut core = self
            .core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let result = operation(&mut core);
        let snapshot = core.snapshot();
        let state = snapshot.into();
        let version = snapshot.version;
        let mut registry = self
            .publication_observers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.retain(|_, observer| observer.strong_count() > 0);
        let notifications = registry
            .iter()
            .filter_map(|(&owner, weak)| {
                let observer = weak.upgrade()?;
                let state = core.publication_state(owner).ok()?;
                observer.prepare(state, false).ok()
            })
            .collect::<Vec<_>>();
        drop(registry);
        let mut pipes = self
            .pipe_observers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pipes.retain(|(input, observer)| {
            observer.strong_count() > 0 && core.pipe_state(*input).is_some()
        });
        let pipe_notifications = pipes
            .iter()
            .filter_map(|(input, weak)| {
                let observer = weak.upgrade()?;
                let state = core.pipe_state(*input)?;
                observer.prepare(state, false).ok()
            })
            .collect::<Vec<_>>();
        drop(pipes);
        let resource_notifications = self.resource_notifications(&core, false);
        drop(core);
        for notification in resource_notifications {
            notification();
        }
        // Foreign cursor wake callbacks may reenter this actor.
        let _ = self.observer.publish_from(version, state);
        for notification in notifications {
            let _ = notification.notify();
        }
        for notification in pipe_notifications {
            let _ = notification.notify();
        }
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
        let core = self
            .core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut state: agent_core::ffi::AgentState = core.snapshot().into();
        state.room = None;
        let publications = {
            let mut registry = self
                .publication_observers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            registry.retain(|_, observer| observer.strong_count() > 0);
            registry
                .iter()
                .filter_map(|(&owner, weak)| {
                    let observer = weak.upgrade()?;
                    let state = core.publication_state(owner).ok()?;
                    observer.prepare(state, true).ok()
                })
                .collect::<Vec<_>>()
        };
        let pipe_notifications = {
            let mut registry = self
                .pipe_observers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            registry.retain(|(_, weak)| weak.strong_count() > 0);
            registry
                .iter()
                .filter_map(|(input, weak)| {
                    let observer = weak.upgrade()?;
                    let state = core.pipe_state(*input)?;
                    observer.prepare(state, true).ok()
                })
                .collect::<Vec<_>>()
        };
        let resource_notifications = self.resource_notifications(&core, true);
        drop(core);
        for notification in resource_notifications {
            notification();
        }
        let _ = self.observer.close(state);
        for notification in publications {
            let _ = notification.notify();
        }
        for notification in pipe_notifications {
            let _ = notification.notify();
        }
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
        if let agent_core::ResourceCommand::DetachPublicationInput { input } = &command {
            return self.detach_input_resource(*input);
        }
        let Some(owner) = (match &command {
            agent_core::ResourceCommand::Release { owner } => Some(*owner),
            _ => None,
        }) else {
            return self.update(|core| core.resource(command));
        };
        let mut core = self
            .core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let final_state = core.publication_state(owner).ok();
        let final_pipes = {
            let pipes = self
                .pipe_observers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pipes
                .iter()
                .filter_map(|(input, observer)| {
                    let observer = observer.upgrade()?;
                    let state = core.pipe_state(*input)?;
                    (state.status != agent_core::ffi::PipeStatus::Closed)
                        .then_some((*input, observer, state))
                })
                .collect::<Vec<_>>()
        };
        let mut resource_notifications = self.resource_notifications(&core, false);
        let result = core.resource(command);
        resource_notifications.extend(self.resource_notifications(&core, false));
        let snapshot = core.snapshot();
        let snapshot_state = snapshot.into();
        let snapshot_version = snapshot.version;
        let mut registry = self
            .publication_observers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        registry.retain(|_, observer| observer.strong_count() > 0);
        let mut publications = Vec::new();
        let mut released_notification = None;
        for (&observed_owner, weak) in registry.iter() {
            let Some(observer) = weak.upgrade() else {
                continue;
            };
            if observed_owner == owner {
                if result.is_ok() {
                    if let Some(state) = final_state.clone() {
                        released_notification = observer.prepare(state, true).ok();
                    }
                }
            } else if let Ok(state) = core.publication_state(observed_owner) {
                if let Ok(notification) = observer.prepare(state, false) {
                    publications.push(notification);
                }
            }
        }
        drop(registry);
        let pipe_notifications = if result.is_ok() {
            self.pipe_observers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|(input, _)| core.pipe_state(*input).is_some());
            final_pipes
                .into_iter()
                .filter_map(|(input, observer, state)| {
                    core.pipe_state(input)
                        .is_none()
                        .then(|| observer.prepare(state, true).ok())
                        .flatten()
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        drop(core);
        for notification in resource_notifications {
            notification();
        }
        let _ = self.observer.publish_from(snapshot_version, snapshot_state);
        if let Some(notification) = released_notification {
            let _ = notification.notify();
        }
        for notification in publications {
            let _ = notification.notify();
        }
        for notification in pipe_notifications {
            let _ = notification.notify();
        }
        result
    }

    fn detach_input_resource(
        &self,
        input: agent_core::PublicationInputId,
    ) -> Result<agent_core::ResourceResult, agent_core::AgentError> {
        let mut core = self
            .core
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let pipe_final = {
            let mut pipes = self
                .pipe_observers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pipes.retain(|(_, weak)| weak.strong_count() > 0);
            pipes.iter().find_map(|(id, weak)| {
                (*id == input)
                    .then(|| weak.upgrade())
                    .flatten()
                    .and_then(|observer| core.pipe_state(input).map(|state| (observer, state)))
            })
        };
        let result =
            core.resource(agent_core::ResourceCommand::DetachPublicationInput { input })?;
        let snapshot = core.snapshot();
        let version = snapshot.version;
        let snapshot_state = snapshot.into();
        let publications = {
            let mut registry = self
                .publication_observers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            registry.retain(|_, weak| weak.strong_count() > 0);
            registry
                .iter()
                .filter_map(|(&owner, weak)| {
                    let observer = weak.upgrade()?;
                    let state = core.publication_state(owner).ok()?;
                    observer.prepare(state, false).ok()
                })
                .collect::<Vec<_>>()
        };
        let pipe_notification = match (&result, pipe_final) {
            (agent_core::ResourceResult::PublicationInput { .. }, Some((observer, state))) => {
                observer.prepare(state, true).ok()
            }
            _ => None,
        };
        drop(core);
        let _ = self.observer.publish_from(version, snapshot_state);
        if let Some(notification) = pipe_notification {
            let _ = notification.notify();
        }
        for notification in publications {
            let _ = notification.notify();
        }
        self.changed.notify_one();
        Ok(result)
    }

    pub(super) fn send_owned_data(
        &self,
        owner: u64,
        expected_join: u64,
        data: Vec<u8>,
    ) -> Result<OperationId, agent_core::AgentError> {
        self.with(|core| {
            let state = agent_core::ffi::AgentState::from(core.snapshot());
            if state.desired_join_revision != expected_join {
                return Err(agent_core::TrackResourceError::NotConnected.into());
            }
            let publication = core.data_publication_state(owner)?;
            core.send_data(
                agent_core::TrackSpec::Data {
                    track_id: publication.track_id,
                    delivery: publication.delivery.into(),
                },
                data,
            )
        })
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
        let notification = self.with(agent_core::Agent::next_notification);
        if let Some(Notification::CatalogWatchTerminated { owner, state }) = &notification {
            self.watch_terminated(*owner, state.clone());
        }
        notification
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

    pub(super) fn has_pipe(&self, input: agent_core::PublicationInputId) -> bool {
        self.with(|core| core.pipe_state(input).is_some())
    }

    pub(super) fn has_input(&self, owner: u64) -> bool {
        self.with(|core| {
            core.publication_state(owner)
                .is_ok_and(|state| state.input_attached)
        })
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

    #[tokio::test]
    async fn publication_projection_tracks_core_revisions_and_closes_outside_lock() {
        use agent_core::{ResourceCommand, ResourceResult, TrackSpec};

        let control = Control::new(
            agent_core::Agent::new(agent_core::AgentConfig {
                endpoint: "http://pulsebeam.test".into(),
                token: "token".into(),
                retry: Default::default(),
                log_level: agent_core::LogLevel::Off,
            })
            .unwrap(),
        );
        let ResourceResult::Publication { owner, .. } = control
            .resource(ResourceCommand::ClaimPublication {
                spec: TrackSpec::Video {
                    track_id: "camera".into(),
                },
            })
            .unwrap()
        else {
            panic!("Core did not create publication")
        };
        let observer = control.publication_observer(owner).unwrap();
        let first = observer.updates().unwrap();
        let second = observer.updates().unwrap();
        let initial = control.publication_state(owner, &observer);
        assert_eq!(first.next().unwrap().await, Some(initial.clone()));
        assert_eq!(second.next().unwrap().await, Some(initial.clone()));
        assert_eq!(
            initial.admission.status,
            agent_core::ffi::PublicationAdmissionStatus::Pending
        );

        let mut pending = Box::pin(first.next().unwrap());
        let waker = std::task::Waker::from(Arc::new(ReenterPublication {
            control: control.clone(),
            owner,
            observer: observer.clone(),
        }));
        let mut context = std::task::Context::from_waker(&waker);
        assert!(pending.as_mut().poll(&mut context).is_pending());
        control
            .resource(ResourceCommand::SetPublicationMuted { owner, muted: true })
            .unwrap();
        let configured = pending.await.unwrap();
        assert_eq!(
            configured.admission.status,
            agent_core::ffi::PublicationAdmissionStatus::Pending
        );
        assert!(!configured.configuration.enabled);
        let revision = configured.configuration.desired_revision;
        control
            .resource(ResourceCommand::SetPublicationMuted { owner, muted: true })
            .unwrap();
        assert_eq!(
            control
                .publication_state(owner, &observer)
                .configuration
                .desired_revision,
            revision
        );
        assert_eq!(
            second
                .next()
                .unwrap()
                .await
                .unwrap()
                .configuration
                .desired_revision,
            revision
        );

        let mut closing = Box::pin(first.next().unwrap());
        assert!(closing.as_mut().poll(&mut context).is_pending());
        control
            .resource(ResourceCommand::Release { owner })
            .unwrap();
        let final_state = closing.await.unwrap();
        assert!(final_state.closed);
        assert!(!final_state.configuration.enabled);
        assert_eq!(first.next().unwrap().await, None);
        assert_eq!(observer.state(), final_state);
    }

    struct ReenterPublication {
        control: Control,
        owner: u64,
        observer: Arc<agent_core::observer::PublicationStateObserver>,
    }

    impl std::task::Wake for ReenterPublication {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            assert!(self.control.core.try_lock().is_ok());
            let _ = self.control.publication_state(self.owner, &self.observer);
        }
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
