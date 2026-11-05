use super::*;
use agent_core::{
    MediaObservationControl, ResourceCommand, ResourceResult, TrackSelector, VideoObservation,
};

pub(super) struct SubscriptionLease {
    owner: u64,
    pub(super) closed: Arc<Signal>,
    commands: mpsc::Sender<Command>,
    changed: Arc<Notify>,
}

impl Drop for SubscriptionLease {
    fn drop(&mut self) {
        self.closed.close();
        self.changed.notify_one();
    }
}

/// A media selection interest, independent of its physical observations.
#[derive(Clone)]
pub struct Subscription {
    inner: Arc<SubscriptionLease>,
}

impl Subscription {
    pub fn is_closed(&self) -> bool {
        self.inner.closed.is_closed()
    }

    pub async fn observe(&self, mode: ReceiveMode) -> Result<RemoteMedia, Error> {
        self.observe_target(None, mode).await
    }

    pub async fn observe_source(
        &self,
        source: String,
        mode: ReceiveMode,
    ) -> Result<RemoteMedia, Error> {
        self.observe_target(Some(source), mode).await
    }

    async fn observe_target(
        &self,
        source: Option<String>,
        mode: ReceiveMode,
    ) -> Result<RemoteMedia, Error> {
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
                source,
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

    pub async fn configure_audio(
        &self,
        playout_delay: agent_core::PlayoutDelay,
    ) -> Result<(), Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        self.resource(ResourceCommand::ConfigureAudio {
            owner: self.inner.owner,
            playout_delay,
        })
        .await
    }

    pub async fn configure_audio_policy(
        &self,
        policy: agent_core::AudioSubscription,
    ) -> Result<(), Error> {
        if self.is_closed() {
            return Err(Error::Closed);
        }
        self.resource(ResourceCommand::ConfigureAudioPolicy {
            owner: self.inner.owner,
            policy,
        })
        .await
    }

    pub async fn close(&self) -> Result<(), Error> {
        if self.is_closed() {
            return Ok(());
        }
        self.inner.closed.close();
        self.inner.changed.notify_one();
        self.resource(ResourceCommand::Release {
            owner: self.inner.owner,
        })
        .await
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

    pub(super) fn claim_configured_audio_subscription(
        &mut self,
        selector: TrackSelector,
        playout_delay: agent_core::PlayoutDelay,
    ) -> Result<Subscription, Error> {
        let subscription = self.claim_audio_subscription(selector)?;
        if let Err(error) = self.core.resource(ResourceCommand::ConfigureAudio {
            owner: subscription.inner.owner,
            playout_delay,
        }) {
            subscription.inner.closed.close();
            self.retire_subscriptions();
            return Err(error.into());
        }
        Ok(subscription)
    }

    pub(super) fn claim_audio_policy(
        &mut self,
        policy: agent_core::AudioSubscription,
    ) -> Result<Subscription, Error> {
        let ResourceResult::Owned { owner } = self
            .core
            .resource(ResourceCommand::ClaimAudioPolicy { policy })?
        else {
            return Err(Error::Rtc("Core did not claim an audio policy".into()));
        };
        Ok(self.subscription_handle(owner))
    }

    fn subscription_handle(&mut self, owner: u64) -> Subscription {
        let inner = Arc::new(SubscriptionLease {
            owner,
            closed: Arc::new(Signal::default()),
            commands: self.command_tx.clone(),
            changed: self.leases_changed.clone(),
        });
        self.subscriptions.insert(owner, Arc::downgrade(&inner));
        Subscription { inner }
    }

    pub(super) fn observe_subscription(
        &mut self,
        owner: u64,
        source: Option<String>,
        mode: ReceiveMode,
        response: oneshot::Sender<Result<RemoteMedia, Error>>,
    ) {
        if response.is_closed() {
            return;
        }
        let command = match source {
            Some(source) => ResourceCommand::BeginAudioSourceObservation { owner, source },
            None => ResourceCommand::BeginMediaObservation { owner },
        };
        let result = self.core.resource(command);
        match result {
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
