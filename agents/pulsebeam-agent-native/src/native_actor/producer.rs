use super::publication::{InputLease, InputTarget};
use super::*;
pub(super) use crate::native_source::SourceKind;

fn ownership_conflict() -> Error {
    Error::Core(agent_core::AgentError::InvalidResource(
        agent_core::TrackResourceError::AlreadyOwned,
    ))
}

pub(super) struct ProducerLease {
    pub(super) owner: u64,
    pub(super) kind: SourceKind,
    #[cfg(feature = "sim")]
    grouped_input: bool,
    pub(super) binding_owner: std::sync::atomic::AtomicU64,
    pipe_reservation: std::sync::Mutex<Option<agent_core::PublicationInputId>>,
    pub(super) closed: Arc<Signal>,
    feedback: broadcast::Sender<ProducerFeedback>,
    target: watch::Receiver<Option<SourceIdentity>>,
    binding: watch::Receiver<Option<InputTarget>>,
    commands: mpsc::Sender<Command>,
    media: mpsc::Sender<MediaCommand>,
    changed: Arc<Notify>,
}

impl ProducerLease {
    pub(super) fn release_pipe_reservation(
        &self,
        owner: u64,
        input: agent_core::PublicationInputId,
    ) {
        let mut reservation = self
            .pipe_reservation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *reservation == Some(input) {
            let _ =
                self.binding_owner
                    .compare_exchange(owner, 0, Ordering::AcqRel, Ordering::Acquire);
            *reservation = None;
        }
    }
}

impl Drop for ProducerLease {
    fn drop(&mut self) {
        self.closed.close();
        self.changed.notify_one();
    }
}

/// An independently owned native producer, not a publication submission port.
#[derive(Clone)]
pub struct ProducerInput {
    pub(super) inner: Arc<ProducerLease>,
}

impl ProducerInput {
    pub fn is_closed(&self) -> bool {
        self.inner.closed.is_closed()
    }

    /// Synchronously reserve this Agent-scoped producer for one publication.
    pub fn pipe_to(&self, publication: &Publication) -> Result<Pipe, Error> {
        if self.is_closed() || publication.is_closed() {
            return Err(Error::Closed);
        }
        if !self
            .inner
            .commands
            .same_channel(&publication.inner.commands)
        {
            return Err(ownership_conflict());
        }
        let compatible = matches!(
            (&publication.inner.spec, self.inner.kind),
            (agent_core::TrackSpec::Audio { .. }, SourceKind::Audio(_))
                | (agent_core::TrackSpec::Video { .. }, SourceKind::Video(_))
        );
        if !compatible {
            return Err(Error::InvalidInput(
                "producer kind differs from the publication".into(),
            ));
        }
        let serial = publication
            .inner
            .pipe_serial
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.is_closed() || publication.is_closed() {
            return Err(Error::Closed);
        }
        self.inner
            .binding_owner
            .compare_exchange(
                0,
                publication.inner.owner,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| ownership_conflict())?;
        let (operation, observer) = match publication
            .inner
            .control
            .attach_pipe(publication.inner.owner, &publication.inner.closed)
        {
            Ok(attached) => attached,
            Err(error) => {
                self.inner.binding_owner.store(0, Ordering::Release);
                return Err(error);
            }
        };
        let Some(input_id) = operation.input() else {
            self.inner.binding_owner.store(0, Ordering::Release);
            return Err(Error::Closed);
        };
        *self
            .inner
            .pipe_reservation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(input_id);
        let (target_tx, target) = watch::channel(None);
        let (feedback, _) = broadcast::channel(EVENTS);
        let inner = Arc::new(InputLease {
            operation: operation.clone(),
            closed: Arc::new(Signal::default()),
            feedback,
            target,
            commands: self.inner.commands.clone(),
            changed: self.inner.changed.clone(),
        });
        let input = PublicationInput { inner };
        let pipe = Pipe::new(
            operation,
            input,
            input_id,
            observer,
            publication.inner.control.clone(),
            self.inner.clone(),
            publication.inner.pipe.clone(),
        );
        let queued = self.inner.commands.try_send(Command::InstallPipe {
            producer: self.clone(),
            input: pipe.handle.input.clone(),
            target: target_tx,
        });
        drop(serial);
        publication
            .inner
            .control
            .notify_pipe_attached(publication.inner.owner);
        match queued {
            Ok(()) => Ok(pipe),
            Err(error) => {
                let _ = publication
                    .inner
                    .control
                    .detach_pipe(&pipe.handle.operation, &pipe.handle.observer);
                self.inner
                    .release_pipe_reservation(publication.inner.owner, input_id);
                pipe.handle.input.inner.closed.close();
                match error {
                    mpsc::error::TrySendError::Full(_) => Err(Error::Backpressure),
                    mpsc::error::TrySendError::Closed(_) => Err(Error::Closed),
                }
            }
        }
    }

    fn target(&self) -> Result<SourceIdentity, Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        self.inner.target.borrow().clone().ok_or(Error::NotReady)
    }

    pub async fn ready(&self) -> Result<(), Error> {
        let mut target = self.inner.target.clone();
        loop {
            if self.is_closed() {
                return Err(Error::Closed);
            }
            if target.borrow().is_some() {
                return Ok(());
            }
            tokio::select! {
                _ = self.inner.closed.wait() => return Err(Error::Closed),
                changed = target.changed() => if changed.is_err() { return Err(Error::Closed); },
            }
        }
    }

    /// Acknowledges native producer admission, including while detached, not delivery.
    pub async fn submit(&self, frame: InputFrame) -> Result<(), Error> {
        #[cfg(not(feature = "sim"))]
        let grouped_input = false;
        #[cfg(feature = "sim")]
        let grouped_input = self.inner.grouped_input;
        if !grouped_input && matches!(frame, InputFrame::H264Simulcast { .. }) {
            return Err(Error::InvalidInput(
                "video publications accept one access unit per frame".into(),
            ));
        }
        let identity = self.target()?;
        validate_frame(&frame)?;
        let (response, result) = oneshot::channel();
        self.inner
            .media
            .try_send(MediaCommand::Producer(ProducerSubmission {
                owner: self.inner.owner,
                identity,
                frame,
                response,
            }))
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => Error::Backpressure,
                mpsc::error::TrySendError::Closed(_) => Error::Closed,
            })?;
        result.await.map_err(|_| Error::Closed)?
    }

    pub async fn capture_time_us(&self) -> Result<i64, Error> {
        let identity = self.target()?;
        let (response, result) = oneshot::channel();
        self.inner
            .commands
            .send(Command::ProducerCaptureTime {
                owner: self.inner.owner,
                identity,
                response,
            })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }

    /// Applies only to the captured publication binding, never a later borrow.
    pub async fn set_limits(
        &self,
        limits: Vec<EncodingLimits>,
    ) -> Result<Vec<EncodingState>, Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        if limits.len() > 3 {
            return Err(Error::InvalidInput("at most three encoding limits".into()));
        }
        let target = self.inner.binding.borrow().clone().ok_or(Error::NotReady)?;
        target.source.set_limits(limits, target.control).await
    }

    pub fn feedback(&self) -> ObservationStream<ProducerFeedback> {
        ObservationStream::new(self.inner.feedback.subscribe(), self.inner.closed.clone())
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
            .send(Command::CloseProducer {
                owner: self.inner.owner,
                response,
            })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }
}

pub(super) struct ProducerIo {
    pub(super) lease: Weak<ProducerLease>,
    pub(super) kind: SourceKind,
    config: VideoSourceConfig,
    pub(super) physical: Option<SourceIdentity>,
    pub(super) target: watch::Sender<Option<SourceIdentity>>,
    pub(super) binding: Option<(u64, PublicationInput)>,
    bound: watch::Sender<Option<InputTarget>>,
}

pub(super) struct ProducerSubmission {
    owner: u64,
    identity: SourceIdentity,
    frame: InputFrame,
    response: oneshot::Sender<Result<(), Error>>,
}

pub(super) fn validate_frame(frame: &InputFrame) -> Result<(), Error> {
    if matches!(frame, InputFrame::H264Simulcast { frames } if frames.len() != 3) {
        return Err(Error::InvalidInput(
            "simulcast requires three genuine encoding-specific units".into(),
        ));
    }
    let bytes = match frame {
        InputFrame::Pcm { frame } => frame.samples.len().saturating_mul(2),
        InputFrame::I420 { frame } => frame.data.len(),
        InputFrame::Opus { frame } => frame.payload.len(),
        InputFrame::H264 { frame } => frame.payload.len(),
        InputFrame::H264Simulcast { frames } => frames.iter().fold(0usize, |bytes, frame| {
            bytes.saturating_add(frame.payload.len())
        }),
    };
    if bytes > MAX_FRAME_BYTES {
        return Err(Error::InvalidInput(
            "frame exceeds the owned input byte limit".into(),
        ));
    }
    Ok(())
}

impl Agent {
    pub async fn create_audio_input(&self) -> Result<ProducerInput, Error> {
        self.create_producer(None).await
    }

    pub async fn create_video_input(
        &self,
        config: VideoSourceConfig,
    ) -> Result<ProducerInput, Error> {
        self.create_producer(Some(config)).await
    }

    #[cfg(feature = "sim")]
    pub async fn create_simulcast_fixture_input(&self) -> Result<ProducerInput, Error> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(Command::CreateSimulcastFixtureProducer { response })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }

    async fn create_producer(
        &self,
        video: Option<VideoSourceConfig>,
    ) -> Result<ProducerInput, Error> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(Command::CreateProducer { video, response })
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
    pub(super) fn create_producer(
        &mut self,
        video: Option<VideoSourceConfig>,
    ) -> Result<ProducerInput, Error> {
        let (kind, config) = match video {
            Some(config) => {
                config.validate_owned()?;
                (SourceKind::Video(self.config.media.video_input), config)
            }
            None => (
                SourceKind::Audio(self.config.media.audio_input),
                VideoSourceConfig::default(),
            ),
        };
        self.create_producer_with_profile(kind, config)
    }

    #[cfg(feature = "sim")]
    pub(super) fn create_simulcast_fixture_producer(&mut self) -> Result<ProducerInput, Error> {
        let profile = self
            .config
            .media
            .video_publications
            .iter()
            .find(|profile| profile.source.encodings.len() == 3)
            .ok_or_else(|| Error::InvalidInput("no encoded simulcast transport fixture".into()))?;
        self.create_producer_with_profile(
            SourceKind::Video(VideoInput::EncodedH264),
            profile.source.clone(),
        )
    }

    fn create_producer_with_profile(
        &mut self,
        kind: SourceKind,
        config: VideoSourceConfig,
    ) -> Result<ProducerInput, Error> {
        self.retire_producers();
        if self.producers.len() >= MAX_PRODUCER_LEASES {
            return Err(Error::CapacityExceeded);
        }
        crate::native_source::NativeProducer::validate(kind, &config)?;
        let owner = self
            .next_producer
            .checked_add(1)
            .ok_or(Error::Backpressure)?;
        self.next_producer = owner;
        let (target_tx, target) = watch::channel(None);
        let (bound, binding) = watch::channel(None);
        let (feedback, _) = broadcast::channel(EVENTS);
        let inner = Arc::new(ProducerLease {
            owner,
            kind,
            #[cfg(feature = "sim")]
            grouped_input: config.encodings.len() == 3,
            binding_owner: std::sync::atomic::AtomicU64::new(0),
            pipe_reservation: std::sync::Mutex::new(None),
            closed: Arc::new(Signal::default()),
            feedback,
            target,
            binding,
            commands: self.command_tx.clone(),
            media: self.media_tx.clone(),
            changed: self.leases_changed.clone(),
        });
        self.producers.insert(
            owner,
            ProducerIo {
                lease: Arc::downgrade(&inner),
                kind,
                config,
                physical: None,
                target: target_tx,
                binding: None,
                bound,
            },
        );
        if let Err(error) = self.ensure_producer(owner) {
            self.producers.remove(&owner);
            return Err(error);
        }
        Ok(ProducerInput { inner })
    }

    pub(super) fn ensure_producer(&mut self, owner: u64) -> Result<Option<SourceIdentity>, Error> {
        let producer = self.producers.get_mut(&owner).ok_or(Error::Closed)?;
        let lease = producer
            .lease
            .upgrade()
            .filter(|lease| !lease.closed.is_closed())
            .ok_or(Error::Closed)?;
        if let Some(identity) = &producer.physical {
            if self
                .peer
                .as_ref()
                .is_none_or(|peer| !peer.producer_is_live(identity))
            {
                return Err(Error::Closed);
            }
            return Ok(Some(identity.clone()));
        }
        let Some(peer) = &mut self.peer else {
            return Ok(None);
        };
        let identity = peer.create_producer(producer.kind, &producer.config)?;
        producer.physical = Some(identity.clone());
        producer.target.send_replace(Some(identity.clone()));
        drop(lease);
        Ok(Some(identity))
    }

    pub(super) fn prepare_producers(&mut self) {
        let owners: Vec<_> = self.producers.keys().copied().collect();
        for owner in owners {
            if let Err(error) = self.ensure_producer(owner) {
                if !matches!(&error, Error::Closed) {
                    self.fail(error);
                }
                if let Err(error) = self.close_producer(owner) {
                    self.fail(error);
                }
            }
        }
    }

    pub(super) fn producer_current(
        &self,
        owner: u64,
        identity: &SourceIdentity,
    ) -> Result<(), Error> {
        self.producers
            .get(&owner)
            .filter(|producer| {
                producer.physical.as_ref() == Some(identity)
                    && producer
                        .lease
                        .upgrade()
                        .is_some_and(|lease| !lease.closed.is_closed())
            })
            .map(|_| ())
            .ok_or(Error::Closed)
    }

    pub(super) fn submit_media(&mut self, command: MediaCommand) {
        match command {
            MediaCommand::Publication(submission) => {
                let result = if !self.input_submission_current(&submission.control) {
                    Err(Error::Closed)
                } else {
                    self.peer.as_mut().ok_or(Error::Closed).and_then(|peer| {
                        peer.submit(&submission.slot, &submission.identity, submission.frame)
                            .map_err(Into::into)
                    })
                };
                let _ = submission.response.send(result);
            }
            MediaCommand::Producer(submission) => {
                let result = self
                    .producer_current(submission.owner, &submission.identity)
                    .and_then(|()| {
                        self.peer
                            .as_mut()
                            .ok_or(Error::Closed)?
                            .submit_producer(&submission.identity, submission.frame)
                            .map_err(Into::into)
                    });
                let _ = submission.response.send(result);
            }
        }
    }

    fn detach_producer_binding(&mut self, owner: u64) -> Result<(), Error> {
        let binding = self
            .producers
            .get_mut(&owner)
            .and_then(|producer| producer.binding.take());
        if let Some((publication, binding)) = binding {
            if let Some(lease) = self
                .producers
                .get(&owner)
                .and_then(|producer| producer.lease.upgrade())
            {
                lease
                    .binding_owner
                    .compare_exchange(publication, 0, Ordering::AcqRel, Ordering::Acquire)
                    .ok();
            }
            let operation = &binding.inner.operation;
            if self
                .inputs
                .get(&publication)
                .is_some_and(|registered| registered.operation == *operation)
            {
                self.close_input(operation.clone())?;
                self.clear_input_target(publication);
                self.inputs.remove(&publication);
            }
        }
        Ok(())
    }

    pub(super) fn close_producer(&mut self, owner: u64) -> Result<(), Error> {
        let detached = self.detach_producer_binding(owner);
        let Some(producer) = self.producers.remove(&owner) else {
            return detached;
        };
        if let Some(lease) = producer.lease.upgrade() {
            lease.closed.close();
        }
        producer.target.send_replace(None);
        let closed = if let (Some(peer), Some(identity)) = (&mut self.peer, producer.physical) {
            peer.close_producer(&identity).map_err(Error::from)
        } else {
            Ok(())
        };
        detached.and(closed)
    }

    pub(super) fn retire_producers(&mut self) {
        let dropped: Vec<_> = self
            .producers
            .iter()
            .filter(|(_, producer)| {
                producer
                    .lease
                    .upgrade()
                    .is_none_or(|lease| lease.closed.is_closed())
            })
            .map(|(&owner, _)| owner)
            .collect();
        for owner in dropped {
            if let Err(error) = self.close_producer(owner) {
                self.fail(error);
            }
        }
        let unbound: Vec<_> = self
            .producers
            .iter()
            .filter(|(_, producer)| {
                producer.binding.as_ref().is_some_and(|(owner, binding)| {
                    binding.is_closed()
                        || self
                            .publications
                            .get(owner)
                            .and_then(Weak::upgrade)
                            .is_none_or(|lease| lease.closed.is_closed())
                })
            })
            .map(|(&owner, _)| owner)
            .collect();
        for owner in unbound {
            if let Err(error) = self.detach_producer_binding(owner) {
                self.fail(error);
            }
        }
    }
}

impl ProducerIo {
    pub(super) fn retire_target(&mut self) {
        self.physical = None;
        self.target.send_replace(None);
        self.bound.send_replace(None);
    }

    pub(super) fn bind_target(&self, target: InputTarget) {
        self.bound.send_replace(Some(target));
    }

    pub(super) fn clear_target(&self, owner: u64) {
        if self
            .bound
            .borrow()
            .as_ref()
            .is_some_and(|target| target.control.operation.owner() == owner)
        {
            self.bound.send_replace(None);
        }
    }

    pub(super) fn is_closed(&self) -> bool {
        self.lease
            .upgrade()
            .is_none_or(|lease| lease.closed.is_closed())
    }

    pub(super) fn feedback(&self, feedback: &ProducerFeedback) {
        if let Some(lease) = self.lease.upgrade() {
            let _ = lease.feedback.send(feedback.clone());
        }
    }
}
