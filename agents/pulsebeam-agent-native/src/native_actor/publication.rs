use super::*;
use agent_core::{
    PublicationInputControl, PublicationInputIntent, PublicationInputOperation, ResourceCommand,
    ResourceResult, TrackSpec,
};

pub(super) struct PublicationLease {
    pub(super) owner: u64,
    pub(super) spec: TrackSpec,
    pub(super) closed: Arc<Signal>,
    readiness: watch::Sender<Option<PhysicalSource>>,
    commands: mpsc::Sender<Command>,
    changed: Arc<Notify>,
}

impl Drop for PublicationLease {
    fn drop(&mut self) {
        self.closed.close();
        self.changed.notify_one();
    }
}

#[derive(Clone)]
pub struct Publication {
    pub(super) inner: Arc<PublicationLease>,
}

impl Publication {
    pub fn is_closed(&self) -> bool {
        self.inner.closed.is_closed()
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
        let (response, result) = oneshot::channel();
        self.inner
            .commands
            .send(Command::Resource {
                command: ResourceCommand::SetPublicationMuted {
                    owner: self.inner.owner,
                    muted: !configuration.enabled,
                },
                response,
            })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)??;
        Ok(())
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
    target: watch::Receiver<Option<InputTarget>>,
    commands: mpsc::Sender<Command>,
    changed: Arc<Notify>,
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

impl<R, F, O> Actor<R, F, O>
where
    R: rtc::SessionRuntime,
    F: FnMut() -> Result<(R, O), Error>,
{
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
        let inner = Arc::new(PublicationLease {
            owner,
            spec,
            closed: Arc::new(Signal::default()),
            readiness: watch::channel(None).0,
            commands: self.command_tx.clone(),
            changed: self.leases_changed.clone(),
        });
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
                pending_control: None,
            },
        );
        self.core
            .resource(ResourceCommand::CompletePublicationInput {
                operation,
                succeeded: true,
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
        match self.core.resource(ResourceCommand::BeginPublicationInput {
            owner: operation.owner(),
            input: PublicationInputIntent::Retain(input),
            present: false,
            muted: None,
        }) {
            Ok(ResourceResult::PublicationInput { operation, .. }) => {
                self.core
                    .resource(ResourceCommand::CompletePublicationInput {
                        operation,
                        succeeded: true,
                    })?;
            }
            Err(agent_core::AgentError::InvalidResource(
                agent_core::TrackResourceError::Released,
            )) => {}
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
                        succeeded: true,
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
                        succeeded: true,
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
                self.fail(error);
                if let Err(error) =
                    self.core
                        .resource(ResourceCommand::CompletePublicationInputTarget {
                            control,
                            succeeded: false,
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
