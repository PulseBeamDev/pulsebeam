use super::publication::InputTarget;
use super::*;
use crate::native_source::SourceKind;

pub(super) struct ProducerLease {
    owner: u64,
    pub(super) closed: Arc<Signal>,
    feedback: broadcast::Sender<ProducerFeedback>,
    target: watch::Receiver<Option<SourceIdentity>>,
    binding: watch::Receiver<Option<InputTarget>>,
    commands: mpsc::Sender<Command>,
    media: mpsc::Sender<MediaCommand>,
    changed: Arc<Notify>,
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
    inner: Arc<ProducerLease>,
}

impl ProducerInput {
    pub fn is_closed(&self) -> bool {
        self.inner.closed.is_closed()
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
    kind: SourceKind,
    config: VideoSourceConfig,
    pub(super) physical: Option<SourceIdentity>,
    pub(super) target: watch::Sender<Option<SourceIdentity>>,
    binding: Option<(u64, PublicationInput)>,
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
            Some(config) => (SourceKind::Video(self.config.media.video_input), config),
            None => (
                SourceKind::Audio(self.config.media.audio_input),
                VideoSourceConfig::default(),
            ),
        };
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

    pub(super) fn bind_producer(
        &mut self,
        owner: u64,
        input: Option<ProducerInput>,
    ) -> Result<(), Error> {
        let publication = self
            .publications
            .get(&owner)
            .and_then(Weak::upgrade)
            .filter(|lease| !lease.closed.is_closed())
            .ok_or(Error::Closed)?;
        let Some(input) = input else {
            if let Some(registered) = self.inputs.get(&owner) {
                let operation = registered.operation.clone();
                self.close_input(operation)?;
                self.clear_input_target(owner);
                if let Some(registered) = self.inputs.remove(&owner)
                    && let Some(lease) = registered.lease.upgrade()
                {
                    lease.closed.close();
                }
            }
            self.retire_producers();
            return Ok(());
        };
        let producer = self
            .producers
            .get(&input.inner.owner)
            .filter(|producer| {
                Weak::ptr_eq(&producer.lease, &Arc::downgrade(&input.inner)) && !input.is_closed()
            })
            .ok_or(Error::Closed)?;
        let compatible = matches!(
            (&publication.spec, producer.kind),
            (agent_core::TrackSpec::Audio { .. }, SourceKind::Audio(_))
                | (agent_core::TrackSpec::Video { .. }, SourceKind::Video(_))
        );
        if !compatible {
            return Err(Error::InvalidInput(
                "producer kind differs from the publication".into(),
            ));
        }
        if producer
            .binding
            .as_ref()
            .is_some_and(|(bound, binding)| *bound != owner && !binding.is_closed())
        {
            return Err(Error::InvalidInput(
                "producer is already borrowed by another publication".into(),
            ));
        }
        let binding = self.register_input(owner)?;
        self.inputs.get_mut(&owner).ok_or(Error::Closed)?.producer = Some(input.inner.owner);
        self.producers
            .get_mut(&input.inner.owner)
            .ok_or(Error::Closed)?
            .binding = Some((owner, binding));
        Ok(())
    }

    fn detach_producer_binding(&mut self, owner: u64) -> Result<(), Error> {
        let binding = self
            .producers
            .get_mut(&owner)
            .and_then(|producer| producer.binding.take());
        if let Some((publication, binding)) = binding {
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
