use super::*;
use agent_core::{
    PublicationInputControl, PublicationInputIntent, PublicationInputOperation, ResourceCommand,
    ResourceResult, TrackSpec,
};

pub(super) struct PublicationLease {
    pub(super) owner: u64,
    pub(super) spec: TrackSpec,
    pub(super) closed: Arc<Signal>,
    closing: std::sync::atomic::AtomicBool,
    readiness: watch::Sender<Option<PhysicalSource>>,
    pub(super) commands: mpsc::Sender<Command>,
    changed: Arc<Notify>,
    pub(super) control: PublicationControl,
    observer: Arc<agent_core::observer::PublicationStateObserver>,
    pub(super) pipe: Arc<std::sync::Mutex<Option<Weak<PipeLease>>>>,
    pub(super) pipe_serial: std::sync::Mutex<()>,
}

impl Drop for PublicationLease {
    fn drop(&mut self) {
        self.closed.close();
        self.changed.notify_one();
    }
}

pub struct PublicationUpdates {
    updates: agent_core::observer::PublicationUpdates,
    _observer: Arc<agent_core::observer::PublicationStateObserver>,
}

impl PublicationUpdates {
    pub async fn next(
        &self,
    ) -> Result<Option<agent_core::ffi::PublicationState>, agent_core::observer::ConcurrentNext>
    {
        Ok(self.updates.next()?.await)
    }

    pub fn close(&self) {
        self.updates.close();
    }
}

#[derive(Clone)]
pub struct Publication {
    pub(super) inner: Arc<PublicationLease>,
}

impl PublicationLease {
    pub(super) fn new(
        owner: u64,
        spec: TrackSpec,
        commands: mpsc::Sender<Command>,
        changed: Arc<Notify>,
        control: PublicationControl,
        observer: Arc<agent_core::observer::PublicationStateObserver>,
    ) -> Arc<Self> {
        Arc::new(Self {
            owner,
            spec,
            closed: Arc::new(Signal::default()),
            closing: std::sync::atomic::AtomicBool::new(false),
            readiness: watch::channel(None).0,
            commands,
            changed,
            control,
            observer,
            pipe: Arc::new(std::sync::Mutex::new(None)),
            pipe_serial: std::sync::Mutex::new(()),
        })
    }
}

impl Publication {
    pub fn is_closed(&self) -> bool {
        self.inner.closing.load(Ordering::Acquire) || self.inner.closed.is_closed()
    }

    pub async fn ready(&self) -> Result<(), Error> {
        let mut readiness = self.inner.readiness.subscribe();
        loop {
            if self.is_closed() {
                return Err(Error::Closed);
            }
            if readiness
                .borrow()
                .as_ref()
                .is_some_and(|source| !source.is_closed())
            {
                return Ok(());
            }
            tokio::select! {
                _ = self.inner.closed.wait() => return Err(Error::Closed),
                changed = readiness.changed() => if changed.is_err() { return Err(Error::Closed); },
            }
        }
    }

    #[cfg(test)]
    pub(super) async fn input(&self) -> Result<PublicationInput, Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        let (response, result) = oneshot::channel();
        self.inner
            .commands
            .send(Command::PublicationInput {
                owner: self.inner.owner,
                response,
            })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }

    pub async fn set_source(&self, producer: Option<&ProducerInput>) -> Result<(), Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        let (response, result) = oneshot::channel();
        self.inner
            .commands
            .send(Command::BindProducer {
                owner: self.inner.owner,
                producer: producer.cloned(),
                response,
            })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }

    pub async fn configure(
        &self,
        configuration: agent_core::ffi::PublicationConfig,
    ) -> Result<(), Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        self.inner.control.configure(
            self.inner.owner,
            &self.inner.observer,
            configuration.enabled,
        )?;
        Ok(())
    }

    pub fn state(&self) -> agent_core::ffi::PublicationState {
        self.inner
            .control
            .state(self.inner.owner, &self.inner.observer)
    }

    pub fn updates(&self) -> Result<PublicationUpdates, agent_core::observer::ObserverLimit> {
        let _ = self.state();
        let updates = self.inner.observer.updates()?;
        Ok(PublicationUpdates {
            updates,
            _observer: self.inner.observer.clone(),
        })
    }

    pub async fn close(&self) -> Result<(), Error> {
        let (pipe, already_closed) = {
            let _serial = self
                .inner
                .pipe_serial
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let already_closed =
                self.inner.closing.swap(true, Ordering::AcqRel) || self.inner.closed.is_closed();
            let pipe = self
                .inner
                .pipe
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .and_then(Weak::upgrade);
            (pipe, already_closed)
        };
        if already_closed {
            if let Some(pipe) = pipe {
                Pipe { handle: pipe }.close().await?;
            }
            return Ok(());
        }
        self.inner.closed.close();
        self.inner.changed.notify_one();
        if let Some(pipe) = pipe {
            Pipe { handle: pipe }.close().await?;
        }
        let (response, result) = oneshot::channel();
        self.inner
            .commands
            .send(Command::Resource {
                command: ResourceCommand::Release {
                    owner: self.inner.owner,
                },
                response,
            })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)??;
        Ok(())
    }
}

#[derive(Clone)]
pub(super) struct InputTarget {
    pub(super) source: PhysicalSource,
    pub(super) control: Arc<PublicationInputControl>,
}

pub(super) struct InputLease {
    pub(super) operation: PublicationInputOperation,
    pub(super) closed: Arc<Signal>,
    pub(super) feedback: broadcast::Sender<ProducerFeedback>,
    pub(super) target: watch::Receiver<Option<InputTarget>>,
    pub(super) commands: mpsc::Sender<Command>,
    pub(super) changed: Arc<Notify>,
}

impl Drop for InputLease {
    fn drop(&mut self) {
        self.closed.close();
        self.changed.notify_one();
    }
}

pub(super) struct RegisteredInput {
    pub(super) operation: PublicationInputOperation,
    pub(super) lease: Weak<InputLease>,
    pub(super) target: watch::Sender<Option<InputTarget>>,
    pub(super) producer: Option<u64>,
    pub(super) pipe: bool,
    pending_control: Option<PublicationInputControl>,
}

#[derive(Clone)]
pub(super) struct PublicationInput {
    pub(super) inner: Arc<InputLease>,
}

impl PublicationInput {
    pub fn is_closed(&self) -> bool {
        self.inner.closed.is_closed()
    }
}

#[cfg(test)]
impl PublicationInput {
    pub fn feedback(&self) -> ObservationStream<ProducerFeedback> {
        ObservationStream::new(self.inner.feedback.subscribe(), self.inner.closed.clone())
    }

    pub(super) fn target(&self) -> Result<InputTarget, Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        self.inner
            .target
            .borrow()
            .clone()
            .filter(|target| target.control.enabled && !target.source.is_closed())
            .ok_or(Error::NotReady)
    }

    pub async fn ready(&self) -> Result<(), Error> {
        let mut target = self.inner.target.clone();
        loop {
            if self.is_closed() {
                return Err(Error::Closed);
            }
            if self.target().is_ok() {
                return Ok(());
            }
            tokio::select! {
                _ = self.inner.closed.wait() => return Err(Error::Closed),
                changed = target.changed() => if changed.is_err() { return Err(Error::Closed); },
            }
        }
    }

    pub async fn set_limits(
        &self,
        limits: Vec<EncodingLimits>,
    ) -> Result<Vec<EncodingState>, Error> {
        let target = self.target()?;
        target.source.set_limits(limits, target.control).await
    }

    pub async fn submit(&self, frame: InputFrame) -> Result<(), Error> {
        let target = self.target()?;
        target.source.submit(frame, target.control).await
    }

    pub async fn capture_time_us(&self) -> Result<i64, Error> {
        self.target()?.source.capture_time_us().await
    }

    pub async fn close(&self) -> Result<(), Error> {
        if self.is_closed() {
            return Ok(());
        }
        self.inner.closed.close();
        self.inner.changed.notify_one();
        let (response, result) = oneshot::channel();
        self.inner
            .commands
            .send(Command::ClosePublicationInput {
                operation: self.inner.operation.clone(),
                response,
            })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }
}

pub(super) struct PipeLease {
    pub(super) operation: PublicationInputOperation,
    pub(super) input: PublicationInput,
    pub(super) input_id: agent_core::PublicationInputId,
    pub(super) observer: Arc<agent_core::observer::PipeStateObserver>,
    pub(super) control: PublicationControl,
    pub(super) producer: Arc<super::producer::ProducerLease>,
    cleanup: Mutex<bool>,
}

impl Drop for PipeLease {
    fn drop(&mut self) {
        if *self.cleanup.get_mut() {
            return;
        }
        self.input.inner.closed.close();
        self.input.inner.changed.notify_one();
        let _ = self.control.detach_pipe(&self.operation, &self.observer);
        let (response, _result) = oneshot::channel();
        let _ = self.input.inner.commands.try_send(Command::ClosePipe {
            operation: self.operation.clone(),
            response,
        });
    }
}

#[derive(Clone)]
pub struct Pipe {
    pub(super) handle: Arc<PipeLease>,
}

impl Pipe {
    pub(super) fn new(
        operation: PublicationInputOperation,
        input: PublicationInput,
        input_id: agent_core::PublicationInputId,
        observer: Arc<agent_core::observer::PipeStateObserver>,
        control: PublicationControl,
        producer: Arc<super::producer::ProducerLease>,
        publication_pipe: Arc<std::sync::Mutex<Option<Weak<PipeLease>>>>,
    ) -> Self {
        let handle = Arc::new(PipeLease {
            operation,
            input,
            input_id,
            observer,
            control,
            producer,
            cleanup: Mutex::new(false),
        });
        *publication_pipe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::downgrade(&handle));
        Self { handle }
    }

    pub fn state(&self) -> agent_core::ffi::PipeState {
        self.handle
            .control
            .pipe_state(self.handle.input_id, &self.handle.observer)
    }

    pub fn updates(
        &self,
    ) -> Result<agent_core::observer::PipeUpdates, agent_core::observer::ObserverLimit> {
        let _ = self.state();
        self.handle.observer.updates()
    }

    pub async fn close(&self) -> Result<(), Error> {
        let mut complete = self.handle.cleanup.lock().await;
        if *complete {
            return Ok(());
        }
        self.handle.input.inner.closed.close();
        self.handle.input.inner.changed.notify_one();
        self.handle
            .control
            .detach_pipe(&self.handle.operation, &self.handle.observer)?;
        let (response, result) = oneshot::channel();
        self.handle
            .input
            .inner
            .commands
            .send(Command::ClosePipe {
                operation: self.handle.operation.clone(),
                response,
            })
            .await
            .map_err(|_| Error::Closed)?;
        let outcome = result.await.map_err(|_| Error::Closed)?;
        self.handle
            .producer
            .release_pipe_reservation(self.handle.operation.owner(), self.handle.input_id);
        *complete = true;
        outcome
    }
}

impl<R, F, O> Actor<R, F, O>
where
    R: rtc::SessionRuntime,
    F: FnMut() -> Result<(R, O), Error>,
{
    pub(super) fn install_pipe(
        &mut self,
        producer: ProducerInput,
        input: PublicationInput,
        target: watch::Sender<Option<InputTarget>>,
    ) -> Result<(), Error> {
        let operation = input.inner.operation.clone();
        let owner = operation.owner();
        let input_id = operation.input().ok_or(Error::Closed)?;
        if !self.core.has_pipe(input_id) {
            input.inner.closed.close();
            input.inner.changed.notify_one();
            producer.inner.release_pipe_reservation(owner, input_id);
            return Ok(());
        }
        if input.is_closed() {
            producer.inner.release_pipe_reservation(owner, input_id);
            return Ok(());
        }
        let invalid =
            if producer.is_closed() {
                Some(Error::Closed)
            } else {
                match self
                    .publications
                    .get(&owner)
                    .and_then(Weak::upgrade)
                    .filter(|lease| !lease.closed.is_closed())
                {
                    None => Some(Error::Closed),
                    Some(publication) => {
                        match self.producers.get(&producer.inner.owner).filter(|value| {
                            Weak::ptr_eq(&value.lease, &Arc::downgrade(&producer.inner))
                        }) {
                            None => Some(Error::Closed),
                            Some(producer_io)
                                if producer_io.binding.as_ref().is_some_and(
                                    |(bound, binding)| *bound != owner && !binding.is_closed(),
                                ) =>
                            {
                                Some(Error::Core(agent_core::AgentError::InvalidResource(
                                    agent_core::TrackResourceError::AlreadyOwned,
                                )))
                            }
                            Some(_producer_io)
                                if producer.inner.binding_owner.load(Ordering::Acquire)
                                    != owner =>
                            {
                                Some(Error::Core(agent_core::AgentError::InvalidResource(
                                    agent_core::TrackResourceError::AlreadyOwned,
                                )))
                            }
                            Some(producer_io)
                                if !matches!(
                                    (&publication.spec, producer_io.kind),
                                    (
                                        TrackSpec::Audio { .. },
                                        super::producer::SourceKind::Audio(_)
                                    ) | (
                                        TrackSpec::Video { .. },
                                        super::producer::SourceKind::Video(_)
                                    )
                                ) =>
                            {
                                Some(Error::InvalidInput(
                                    "producer kind differs from the publication".into(),
                                ))
                            }
                            Some(_) => None,
                        }
                    }
                }
            };
        if let Some(error) = invalid {
            self.core
                .resource(ResourceCommand::CompletePublicationInput {
                    operation,
                    result: Err(error.error_info().code),
                })?;
            input.inner.closed.close();
            input.inner.changed.notify_one();
            producer.inner.release_pipe_reservation(owner, input_id);
            return Ok(());
        }
        if self.inputs.get(&owner).is_some_and(|registered| {
            registered.operation != operation
                && registered
                    .operation
                    .input()
                    .is_some_and(|old| self.core.has_pipe(old))
        }) {
            let error = Error::Core(agent_core::AgentError::InvalidResource(
                agent_core::TrackResourceError::AlreadyOwned,
            ));
            self.core
                .resource(ResourceCommand::CompletePublicationInput {
                    operation,
                    result: Err(error.error_info().code),
                })?;
            input.inner.closed.close();
            input.inner.changed.notify_one();
            producer.inner.release_pipe_reservation(owner, input_id);
            return Err(error);
        }
        if self.inputs.contains_key(&owner) {
            self.clear_input_target(owner);
        }
        if let Some(stale) = self.inputs.remove(&owner)
            && let Some(lease) = stale.lease.upgrade()
        {
            lease.closed.close();
        }
        self.inputs.insert(
            owner,
            RegisteredInput {
                operation: operation.clone(),
                lease: Arc::downgrade(&input.inner),
                target,
                producer: Some(producer.inner.owner),
                pipe: true,
                pending_control: None,
            },
        );
        if let Some(value) = self.producers.get_mut(&producer.inner.owner) {
            value.binding = Some((owner, input));
        }
        self.core
            .resource(ResourceCommand::CompletePublicationInput {
                operation,
                result: Ok(()),
            })?;
        Ok(())
    }

    pub(super) fn cleanup_pipe(
        &mut self,
        operation: PublicationInputOperation,
    ) -> Result<(), Error> {
        let owner = operation.owner();
        if self
            .inputs
            .get(&owner)
            .is_some_and(|registered| registered.operation == operation)
        {
            self.clear_input_target(owner);
            let producer_owner = self
                .inputs
                .get(&owner)
                .and_then(|registered| registered.producer);
            if let Some(registered) = self.inputs.remove(&owner)
                && let Some(lease) = registered.lease.upgrade()
            {
                lease.closed.close();
            }
            if let Some(producer) =
                producer_owner.and_then(|producer| self.producers.get_mut(&producer))
                && producer.binding.as_ref().is_some_and(|(bound, binding)| {
                    *bound == owner && binding.inner.operation == operation
                })
            {
                producer.binding = None;
                if let Some(lease) = producer.lease.upgrade() {
                    if let Some(input_id) = operation.input() {
                        lease.release_pipe_reservation(owner, input_id);
                    }
                }
            }
        }
        Ok(())
    }

    pub(super) fn input_submission_current(&self, control: &PublicationInputControl) -> bool {
        control.enabled
            && self.core.publication_input_control_current(control)
            && self
                .publications
                .get(&control.operation.owner())
                .and_then(Weak::upgrade)
                .is_some_and(|lease| !lease.closed.is_closed())
            && self
                .inputs
                .get(&control.operation.owner())
                .is_some_and(|input| {
                    input.producer.is_none_or(|owner| {
                        self.producers
                            .get(&owner)
                            .is_some_and(|producer| !producer.is_closed())
                    })
                })
            && self
                .inputs
                .get(&control.operation.owner())
                .and_then(|input| input.lease.upgrade())
                .is_some_and(|lease| {
                    !lease.closed.is_closed()
                        && lease.operation.input() == control.operation.input()
                })
    }

    pub(super) fn claim_publication(&mut self, spec: TrackSpec) -> Result<Publication, Error> {
        let ResourceResult::Publication { owner, .. } = self
            .core
            .resource(ResourceCommand::ClaimPublication { spec: spec.clone() })?
        else {
            return Err(Error::Rtc("Core did not claim a publication".into()));
        };
        let observer = self.core.publication_observer(owner).map_err(Error::from)?;
        let inner = PublicationLease::new(
            owner,
            spec,
            self.command_tx.clone(),
            self.leases_changed.clone(),
            self.core.publication_control(),
            observer,
        );
        self.publications.insert(owner, Arc::downgrade(&inner));
        Ok(Publication { inner })
    }

    pub(super) fn register_input(&mut self, owner: u64) -> Result<PublicationInput, Error> {
        let ResourceResult::PublicationInput { operation, .. } =
            self.core.resource(ResourceCommand::BeginPublicationInput {
                owner,
                input: PublicationInputIntent::Replace,
                present: true,
                muted: None,
            })?
        else {
            return Err(Error::Rtc(
                "Core did not register a publication input".into(),
            ));
        };
        self.clear_input_target(owner);
        if let Some(previous) = self.inputs.remove(&owner) {
            if let Some(lease) = previous.lease.upgrade() {
                lease.closed.close();
            }
        }
        let (target_tx, target) = watch::channel(None);
        let (feedback, _) = broadcast::channel(EVENTS);
        let inner = Arc::new(InputLease {
            operation: operation.clone(),
            closed: Arc::new(Signal::default()),
            feedback,
            target,
            commands: self.command_tx.clone(),
            changed: self.leases_changed.clone(),
        });
        self.inputs.insert(
            owner,
            RegisteredInput {
                operation: operation.clone(),
                lease: Arc::downgrade(&inner),
                target: target_tx,
                producer: None,
                pipe: false,
                pending_control: None,
            },
        );
        self.core
            .resource(ResourceCommand::CompletePublicationInput {
                operation,
                result: Ok(()),
            })?;
        Ok(PublicationInput { inner })
    }

    pub(super) fn close_input(
        &mut self,
        operation: PublicationInputOperation,
    ) -> Result<(), Error> {
        let Some(input) = operation.input() else {
            return Ok(());
        };
        match self
            .core
            .resource(ResourceCommand::DetachPublicationInput { input })
        {
            Ok(ResourceResult::PublicationInput { operation, .. }) => {
                self.clear_input_target(operation.owner());
                self.core
                    .resource(ResourceCommand::CompletePublicationInput {
                        operation,
                        result: Ok(()),
                    })?;
            }
            Ok(ResourceResult::Superseded) => {}
            Err(error) => return Err(error.into()),
            Ok(_) => return Err(Error::Rtc("Core did not detach a publication input".into())),
        }
        Ok(())
    }

    pub(super) fn progress_publication_inputs(&mut self) {
        let generation = self.peer.as_ref().map(|peer| peer.generation);
        let controls: Vec<_> = self
            .inputs
            .values_mut()
            .filter_map(|input| {
                let control = input.pending_control.as_ref()?;
                if control
                    .target
                    .as_ref()
                    .is_some_and(|target| Some(target.generation) == generation)
                {
                    input.pending_control.take()
                } else {
                    None
                }
            })
            .collect();
        for control in controls {
            self.input_control(control);
        }
    }

    pub(super) fn input_control(&mut self, control: PublicationInputControl) {
        if !self.core.publication_input_control_current(&control) {
            return;
        }
        let owner = control.operation.owner();
        let Some(registered) = self.inputs.get_mut(&owner) else {
            return;
        };
        registered.pending_control = None;
        let Some(lease) = registered.lease.upgrade() else {
            return;
        };
        if lease.operation.input() != control.operation.input() || lease.closed.is_closed() {
            lease.closed.close();
            self.clear_input_target(owner);
            self.inputs.remove(&owner);
            return;
        }
        let producer = registered.producer;
        if producer.is_some() && !control.enabled {
            self.clear_input_target(owner);
            if let Err(error) =
                self.core
                    .resource(ResourceCommand::CompletePublicationInputTarget {
                        control,
                        result: Ok(()),
                    })
            {
                self.fail(error.into());
            }
            return;
        }
        let previous = registered.target.borrow().clone();
        let Some(target) = &control.target else {
            self.clear_input_target(owner);
            return;
        };
        let source = match previous {
            Some(previous)
                if previous.control.target == control.target && !previous.source.is_closed() =>
            {
                Ok(previous.source)
            }
            _ if !control.enabled => {
                self.clear_input_target(owner);
                return;
            }
            _ if self
                .peer
                .as_ref()
                .is_none_or(|peer| peer.generation != target.generation) =>
            {
                self.clear_input_target(owner);
                if let Some(registered) = self.inputs.get_mut(&owner) {
                    registered.pending_control = Some(control);
                }
                return;
            }
            _ => self.create_source(target.binding.slot.clone(), producer),
        };
        let result = source.and_then(|source| {
            let enabled =
                self.peer_mut()?
                    .enable(source.slot(), &source.identity(), control.enabled)?;
            if enabled != control.enabled {
                return Err(Error::Rtc(
                    "native source enable readback differs from the Core request".into(),
                ));
            }
            Ok(source)
        });
        match result {
            Ok(source) => {
                let ready_source = source.clone();
                if let Some(registered) = self.inputs.get(&owner) {
                    let target = InputTarget {
                        source,
                        control: Arc::new(control.clone()),
                    };
                    registered.target.send_replace(Some(target.clone()));
                    if let Some(owner) = registered.producer
                        && let Some(producer) = self.producers.get(&owner)
                    {
                        producer.bind_target(target);
                    }
                }
                let enabled = control.enabled;
                match self
                    .core
                    .resource(ResourceCommand::CompletePublicationInputTarget {
                        control,
                        result: Ok(()),
                    }) {
                    Ok(ResourceResult::Updated) => {
                        self.publication_readiness(owner, enabled.then_some(ready_source));
                    }
                    Ok(_) => self.clear_input_target(owner),
                    Err(error) => {
                        self.clear_input_target(owner);
                        self.fail(error.into());
                    }
                }
            }
            Err(error) => {
                self.clear_input_target(owner);
                let code = error.error_info().code;
                self.fail(error);
                if let Err(error) =
                    self.core
                        .resource(ResourceCommand::CompletePublicationInputTarget {
                            control,
                            result: Err(code),
                        })
                {
                    self.fail(error.into());
                }
            }
        }
    }

    fn publication_readiness(&self, owner: u64, readiness: Option<PhysicalSource>) {
        if let Some(lease) = self.publications.get(&owner).and_then(Weak::upgrade) {
            lease.readiness.send_replace(readiness);
        }
    }

    pub(super) fn clear_input_target(&mut self, owner: u64) {
        self.publication_readiness(owner, None);
        if let Some(registered) = self.inputs.get(&owner)
            && let Some(producer) = registered
                .producer
                .and_then(|producer| self.producers.get(&producer))
        {
            producer.clear_target(owner);
        }
        let previous = self
            .inputs
            .get(&owner)
            .and_then(|registered| registered.target.send_replace(None));
        if let Some(previous) = previous {
            let slot = previous.source.slot();
            let identity = previous.source.identity();
            previous.source.inner.closed.close();
            if let Some(peer) = &mut self.peer {
                if let Err(error) = peer.close_source(slot, &identity) {
                    if error.kind != rtc::PeerErrorKind::Closed {
                        self.fail(error.into());
                    }
                }
            }
            if self
                .sources
                .get(slot)
                .is_some_and(|source| source.identity == identity)
            {
                self.close_source_observation(slot);
            }
        }
    }

    pub(super) fn retire_publications(&mut self) {
        let released: Vec<_> = self
            .publications
            .iter()
            .filter(|(_, lease)| lease.upgrade().is_none_or(|lease| lease.closed.is_closed()))
            .map(|(&owner, _)| owner)
            .collect();
        for owner in released {
            self.publications.remove(&owner);
            if let Err(error) = self.core.resource(ResourceCommand::Release { owner }) {
                self.fail(error.into());
                continue;
            }
            if let Some(input) = self.inputs.remove(&owner) {
                self.clear_input_target(owner);
                if let Some(lease) = input.lease.upgrade() {
                    lease.closed.close();
                }
                if let Some(producer_id) = input.producer
                    && let Some(producer) = self.producers.get_mut(&producer_id)
                    && producer.binding.as_ref().is_some_and(|(bound, binding)| {
                        *bound == owner && binding.inner.operation == input.operation
                    })
                {
                    producer.binding = None;
                    if let Some(lease) = producer.lease.upgrade() {
                        if input.pipe {
                            if let Some(input_id) = input.operation.input() {
                                lease.release_pipe_reservation(owner, input_id);
                            }
                        } else {
                            lease
                                .binding_owner
                                .compare_exchange(owner, 0, Ordering::AcqRel, Ordering::Acquire)
                                .ok();
                        }
                    }
                }
            }
        }
        let dropped: Vec<_> = self
            .inputs
            .values()
            .filter(|input| {
                input
                    .lease
                    .upgrade()
                    .is_none_or(|lease| lease.closed.is_closed())
            })
            .map(|input| input.operation.clone())
            .collect();
        for operation in dropped {
            self.clear_input_target(operation.owner());
            self.inputs.remove(&operation.owner());
            if let Err(error) = self.close_input(operation) {
                self.fail(error);
            }
        }
    }
}
