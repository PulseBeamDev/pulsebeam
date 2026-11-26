use super::*;
#[cfg(test)]
use agent_core::TrackSelector;
use agent_core::{MediaObservationControl, ResourceCommand, ResourceResult, VideoObservation};

pub(super) struct SubscriptionLease {
    owner: u64,
    pub(super) closed: Arc<Signal>,
    commands: mpsc::Sender<Command>,
    changed: Arc<Notify>,
    core: Control,
    pub(super) frame_reader: std::sync::Mutex<Weak<super::frames::FrameReaderLease>>,
}

impl SubscriptionLease {
    fn release(&self) -> Result<(), Error> {
        let reader = self
            .frame_reader
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .upgrade();
        if let Some(reader) = reader {
            reader.close();
        }
        self.core
            .resource(ResourceCommand::Release { owner: self.owner })?;
        self.changed.notify_one();
        Ok(())
    }
}

impl Drop for SubscriptionLease {
    fn drop(&mut self) {
        if !self.closed.is_closed() {
            self.closed.close();
            let _ = self.release();
        }
        self.changed.notify_one();
    }
}

/// A media selection interest, independent of its physical observations.
#[derive(Clone)]
pub struct Subscription {
    pub(super) inner: Arc<SubscriptionLease>,
}

impl Subscription {
    pub(super) fn new(
        owner: u64,
        commands: &mpsc::Sender<Command>,
        core: Control,
        changed: Arc<Notify>,
    ) -> Self {
        Self {
            inner: Arc::new(SubscriptionLease {
                owner,
                closed: Arc::new(Signal::default()),
                commands: commands.clone(),
                changed,
                core,
                frame_reader: std::sync::Mutex::new(Weak::new()),
            }),
        }
    }

    pub(super) fn owner(&self) -> u64 {
        self.inner.owner
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.is_closed()
    }

    pub(crate) fn refresh_state(&self) {
        self.inner.core.state();
    }

    pub(crate) fn video_observer(
        &self,
    ) -> Result<Arc<agent_core::observer::VideoSubscriptionStateObserver>, Error> {
        self.inner
            .core
            .video_subscription_observer(self.inner.owner)
    }

    pub(crate) fn audio_observer(
        &self,
    ) -> Result<Arc<agent_core::observer::AudioSubscriptionStateObserver>, Error> {
        self.inner
            .core
            .audio_subscription_observer(self.inner.owner)
    }

    pub async fn observe(&self, mode: ReceiveMode) -> Result<RemoteMedia, Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        struct Wake(Arc<Notify>);
        impl Drop for Wake {
            fn drop(&mut self) {
                self.0.notify_one();
            }
        }
        let _wake = Wake(self.inner.changed.clone());
        let (response, result) = oneshot::channel();
        self.inner
            .commands
            .send(Command::ObserveSubscription {
                owner: self.inner.owner,
                mode,
                response,
            })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }

    pub async fn configure_video(&self, observation: VideoObservation) -> Result<(), Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        self.resource(ResourceCommand::ObserveVideo {
            owner: self.inner.owner,
            observation,
        })
        .await
    }

    pub async fn close(&self) -> Result<(), Error> {
        if self.is_closed() {
            return Ok(());
        }
        self.inner.closed.close();
        self.inner.release()
    }

    async fn resource(&self, command: ResourceCommand) -> Result<(), Error> {
        let (response, result) = oneshot::channel();
        self.inner
            .commands
            .send(Command::Resource { command, response })
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)??;
        Ok(())
    }
}

pub(super) struct ObservationIo {
    mode: ReceiveMode,
    reply: Option<oneshot::Sender<Result<RemoteMedia, Error>>>,
    control: Option<MediaObservationControl>,
    lease: Weak<StreamLease>,
}

impl<R, F, O> Actor<R, F, O>
where
    R: rtc::SessionRuntime,
    F: FnMut() -> Result<(R, O), Error>,
{
    #[cfg(test)]
    pub(super) fn claim_subscription(
        &mut self,
        selector: TrackSelector,
        observation: VideoObservation,
    ) -> Result<Subscription, Error> {
        let ResourceResult::Owned { owner } = self
            .core
            .resource(ResourceCommand::ClaimVideo { selector })?
        else {
            return Err(Error::Rtc("Core did not claim a video subscription".into()));
        };
        if let Err(error) = self
            .core
            .resource(ResourceCommand::ObserveVideo { owner, observation })
        {
            let _ = self.core.resource(ResourceCommand::Release { owner });
            return Err(error.into());
        }
        Ok(self.subscription_handle(owner))
    }

    #[cfg(test)]
    pub(super) fn claim_audio_subscription(
        &mut self,
        selector: TrackSelector,
    ) -> Result<Subscription, Error> {
        let ResourceResult::Owned { owner } = self
            .core
            .resource(ResourceCommand::ClaimAudioSubscription { selector })?
        else {
            return Err(Error::Rtc(
                "Core did not claim an audio subscription".into(),
            ));
        };
        Ok(self.subscription_handle(owner))
    }

    fn subscription_handle(&mut self, owner: u64) -> Subscription {
        let inner = Arc::new(SubscriptionLease {
            owner,
            closed: Arc::new(Signal::default()),
            commands: self.command_tx.clone(),
            changed: self.leases_changed.clone(),
            core: self.core.clone(),
            frame_reader: std::sync::Mutex::new(Weak::new()),
        });
        self.subscriptions.insert(owner, Arc::downgrade(&inner));
        Subscription { inner }
    }

    pub(super) fn begin_observation(
        &mut self,
        command: ResourceCommand,
        mode: ReceiveMode,
        response: oneshot::Sender<Result<RemoteMedia, Error>>,
    ) {
        if response.is_closed() {
            return;
        }
        match self.core.resource(command) {
            Ok(ResourceResult::MediaObservation(id)) => {
                self.observations.insert(
                    id,
                    ObservationIo {
                        mode,
                        reply: Some(response),
                        control: None,
                        lease: Weak::new(),
                    },
                );
            }
            Ok(_) => {
                let _ = response.send(Err(Error::Rtc("Core did not begin an observation".into())));
            }
            Err(error) => {
                let _ = response.send(Err(error.into()));
            }
        }
    }

    pub(super) fn observation_control(&mut self, control: MediaObservationControl) {
        let id = control.observation;
        if control.closed {
            if let Some(mut io) = self.observations.remove(&id) {
                if let Some(lease) = io.lease.upgrade() {
                    lease.close();
                    self.leases_changed.notify_one();
                }
                if let Some(reply) = io.reply.take() {
                    let _ = reply.send(Err(Error::Closed));
                }
                let _ = self.core.resource(ResourceCommand::Release {
                    owner: id.request(),
                });
            }
        } else if let Some(io) = self.observations.get_mut(&id) {
            io.control = Some(control);
        } else {
            let _ = self.core.resource(ResourceCommand::Release {
                owner: id.request(),
            });
        }
    }

    pub(super) fn progress_observations(&mut self) {
        let pending: Vec<_> = self
            .observations
            .iter()
            .filter_map(|(id, io)| {
                io.reply.as_ref()?;
                Some((*id, io.mode, io.control.clone()?))
            })
            .collect();
        for (id, mode, control) in pending {
            let Some(target) = &control.target else {
                continue;
            };
            // RTC track IDs are platform facts, not the server publication ID.
            // Source ownership is authorized by Core; these are the actual I/O facts.
            let available = self.peer.as_ref().is_some_and(|peer| {
                peer.generation == target.generation
                    && peer
                        .receivers
                        .get(&target.binding.slot)
                        .is_some_and(|receiver| {
                            receiver.track.generation == target.generation.get()
                                && receiver.track.receiver == receiver.id.get()
                                && receiver.track.mid == target.binding.mid
                                && receiver.track.media_index == target.binding.media_index
                        })
            });
            if !available {
                continue;
            }
            if !matches!(
                self.core
                    .resource(ResourceCommand::AuthorizeMediaObservation {
                        control: control.clone(),
                    }),
                Ok(ResourceResult::Updated)
            ) {
                continue;
            }
            let result = self.observe(target.binding.slot.clone(), mode);
            let accepted = matches!(
                self.core
                    .resource(ResourceCommand::CompleteMediaObservation {
                        control: control.clone(),
                        succeeded: result.is_ok(),
                    }),
                Ok(ResourceResult::Updated)
            );
            if !accepted {
                if let Ok(remote) = result {
                    remote.lease.close();
                }
                continue;
            }
            let Some(io) = self.observations.get_mut(&id) else {
                if let Ok(remote) = result {
                    remote.lease.close();
                }
                continue;
            };
            let result = result.map(|mut remote| {
                io.lease = Arc::downgrade(&remote.lease);
                remote.observation = Some(control);
                remote
            });
            if let Some(reply) = io.reply.take() {
                let _ = reply.send(result);
            }
        }
    }

    pub(super) fn retire_subscriptions(&mut self) {
        let dropped: Vec<_> = self
            .subscriptions
            .iter()
            .filter(|(_, lease)| lease.upgrade().is_none_or(|lease| lease.closed.is_closed()))
            .map(|(owner, _)| *owner)
            .collect();
        for owner in dropped {
            let _ = self.core.resource(ResourceCommand::Release { owner });
            self.subscriptions.remove(&owner);
        }
        let ended: Vec<_> = self
            .observations
            .iter()
            .filter(|(_, io)| match &io.reply {
                Some(reply) => reply.is_closed(),
                None => io
                    .lease
                    .upgrade()
                    .is_none_or(|lease| lease.closed.is_closed()),
            })
            .map(|(id, _)| *id)
            .collect();
        for id in ended {
            if let Some(io) = self.observations.remove(&id) {
                if let Some(control) = io.control {
                    let _ = self
                        .core
                        .resource(ResourceCommand::EndMediaObservation { control });
                }
                let _ = self.core.resource(ResourceCommand::Release {
                    owner: id.request(),
                });
            }
        }
    }

    pub(super) fn close_subscription_handles(&mut self) {
        for lease in self.subscriptions.values().filter_map(Weak::upgrade) {
            lease.closed.close();
        }
        self.subscriptions.clear();
        for (_, mut io) in core::mem::take(&mut self.observations) {
            if let Some(lease) = io.lease.upgrade() {
                lease.close();
            }
            if let Some(reply) = io.reply.take() {
                let _ = reply.send(Err(Error::Closed));
            }
        }
    }
}
