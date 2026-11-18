use super::*;
use agent_core::{TrackSelector, TrackSpec, VideoObservation};

#[derive(Clone)]
struct MediaAddress {
    spec: TrackSpec,
    source: Option<TrackSelector>,
    commands: mpsc::Sender<Command>,
    core: Control,
    closed: Arc<Signal>,
    finished: Arc<Signal>,
}

impl MediaAddress {
    fn new(
        spec: TrackSpec,
        source: Option<TrackSelector>,
        commands: &mpsc::Sender<Command>,
        core: &Control,
        closed: &Arc<Signal>,
        finished: &Arc<Signal>,
    ) -> Result<Self, Error> {
        if closed.is_closed() || finished.is_closed() {
            return Err(Error::Closed);
        }
        spec.validate()
            .map_err(|error| Error::Core(agent_core::AgentError::InvalidResource(error.into())))?;
        if let Some(source) = &source {
            source.validate().map_err(|error| {
                Error::Core(agent_core::AgentError::InvalidResource(error.into()))
            })?;
        }
        Ok(Self {
            spec,
            source,
            commands: commands.clone(),
            core: core.clone(),
            closed: closed.clone(),
            finished: finished.clone(),
        })
    }

    fn selector(&self) -> Result<TrackSelector, Error> {
        self.source.clone().ok_or_else(|| {
            Error::InvalidInput("local media does not identify a remote source".into())
        })
    }

    fn claim(
        &self,
        selector: TrackSelector,
        configuration: control::SubscriptionConfiguration,
    ) -> Result<Subscription, Error> {
        if self.closed.is_closed() || self.finished.is_closed() {
            return Err(Error::Closed);
        }
        let owner = self.core.claim_subscription(selector, configuration)?;
        let subscription = Subscription::new(
            owner,
            &self.commands,
            self.core.clone(),
            self.core.changed.clone(),
        );
        if self.closed.is_closed() || self.finished.is_closed() {
            drop(subscription);
            return Err(Error::Closed);
        }
        match self.commands.try_send(Command::RegisterSubscription {
            subscription: subscription.clone(),
        }) {
            Ok(()) => {
                self.core.changed.notify_one();
                Ok(subscription)
            }
            Err(error) => {
                drop(subscription);
                match error {
                    mpsc::error::TrySendError::Full(_) => Err(Error::Backpressure),
                    mpsc::error::TrySendError::Closed(_) => Err(Error::Closed),
                }
            }
        }
    }
}

macro_rules! local_track {
    ($name:ident, $kind:ident, $lookup:ident) => {
        #[derive(Clone)]
        pub struct $name {
            address: MediaAddress,
            core: Control,
        }

        impl $name {
            pub(crate) fn declare(
                &self,
                configuration: agent_core::ffi::PublicationConfig,
            ) -> Result<Publication, Error> {
                if self.address.closed.is_closed() || self.address.finished.is_closed() {
                    return Err(Error::Closed);
                }
                let (owner, observer) = self
                    .core
                    .declare_publication(self.address.spec.clone(), configuration.enabled)?;
                let inner = PublicationLease::new(
                    owner,
                    self.address.spec.clone(),
                    self.address.commands.clone(),
                    self.core.changed.clone(),
                    self.core.publication_control(),
                    observer,
                );
                let publication = Publication { inner };
                if self.address.closed.is_closed() || self.address.finished.is_closed() {
                    publication.inner.closed.close();
                    self.core.revoke_publication(owner);
                    return Err(Error::Closed);
                }
                match self
                    .address
                    .commands
                    .try_send(Command::RegisterPublication {
                        publication: publication.clone(),
                    }) {
                    Ok(()) => {
                        self.core.changed.notify_one();
                        Ok(publication)
                    }
                    Err(error) => {
                        publication.inner.closed.close();
                        self.core.revoke_publication(owner);
                        match error {
                            mpsc::error::TrySendError::Full(_) => Err(Error::Backpressure),
                            mpsc::error::TrySendError::Closed(_) => Err(Error::Closed),
                        }
                    }
                }
            }

            pub fn publish(
                &self,
                configuration: agent_core::ffi::PublicationConfig,
            ) -> Result<Publication, Error> {
                self.declare(configuration)
            }
        }

        impl Agent {
            pub fn $lookup(&self, label: String) -> Result<$name, Error> {
                Ok($name {
                    address: MediaAddress::new(
                        TrackSpec::$kind { label },
                        None,
                        &self.commands,
                        &self.core,
                        &self.lease.close,
                        &self.lease.finished,
                    )?,
                    core: self.core.clone(),
                })
            }
        }
    };
}

local_track!(LocalVideoTrack, Video, video);
local_track!(LocalAudioTrack, Audio, audio);

#[derive(Clone)]
pub struct RemoteVideoTrack {
    address: MediaAddress,
}

impl RemoteVideoTrack {
    pub fn subscribe(&self, observation: VideoObservation) -> Result<Subscription, Error> {
        let selector = self.address.selector()?;
        self.address.claim(
            selector,
            control::SubscriptionConfiguration::Video(observation),
        )
    }
}

#[derive(Clone)]
pub struct RemoteAudioTrack {
    address: MediaAddress,
}

impl RemoteAudioTrack {
    pub fn subscribe(
        &self,
        configuration: agent_core::ffi::AudioSubscriptionConfig,
    ) -> Result<Subscription, Error> {
        let selector = self.address.selector()?;
        self.address.claim(
            selector,
            control::SubscriptionConfiguration::Audio(configuration.playout_delay.into()),
        )
    }
}

impl Participant {
    pub fn video(&self, label: String) -> Result<RemoteVideoTrack, Error> {
        let source = self.source(label.clone());
        Ok(RemoteVideoTrack {
            address: MediaAddress::new(
                TrackSpec::Video { label },
                Some(source),
                &self.commands,
                &self.core,
                &self.closed,
                &self.finished,
            )?,
        })
    }

    pub fn audio(&self, label: String) -> Result<RemoteAudioTrack, Error> {
        let source = self.source(label.clone());
        Ok(RemoteAudioTrack {
            address: MediaAddress::new(
                TrackSpec::Audio { label },
                Some(source),
                &self.commands,
                &self.core,
                &self.closed,
                &self.finished,
            )?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn subscription_claim_is_local_and_close_fences_queued_actor_registration() {
        let core = Control::new(
            agent_core::Agent::new(agent_core::AgentConfig {
                endpoint: "http://pulsebeam.test".into(),
                token: "token".into(),
                retry: Default::default(),
                log_level: agent_core::LogLevel::Off,
            })
            .unwrap(),
        );
        let (commands, mut queued) = mpsc::channel(4);
        let closed = Arc::new(Signal::default());
        let finished = Arc::new(Signal::default());
        let selector = TrackSelector {
            room: "pinned-room".into(),
            participant_external_id: "remote-endpoint".into(),
            label: "camera".into(),
        };
        let address = MediaAddress::new(
            TrackSpec::Video {
                label: selector.label.clone(),
            },
            Some(selector.clone()),
            &commands,
            &core,
            &closed,
            &finished,
        )
        .unwrap();

        assert_eq!(core.snapshot().desired_room, None);
        let first = address
            .claim(
                selector.clone(),
                control::SubscriptionConfiguration::Video(Default::default()),
            )
            .unwrap();
        assert!(!first.is_closed());
        first.close().await.unwrap();
        assert!(first.is_closed());
        let Command::RegisterSubscription { subscription } = queued.try_recv().unwrap() else {
            panic!("synchronous subscription did not queue actor registration")
        };
        assert!(
            subscription.is_closed(),
            "stale actor continuation saw live demand"
        );
        drop(subscription);

        let replacement = address
            .claim(
                selector,
                control::SubscriptionConfiguration::Video(Default::default()),
            )
            .unwrap();
        assert!(!replacement.is_closed());
        replacement.close().await.unwrap();
    }
}
