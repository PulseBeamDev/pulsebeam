use super::*;
use agent_core::{TrackSelector, TrackSpec, VideoObservation};

#[derive(Clone)]
struct MediaAddress {
    spec: TrackSpec,
    participant_id: Option<String>,
    commands: mpsc::Sender<Command>,
    closed: Arc<Signal>,
    finished: Arc<Signal>,
}

impl MediaAddress {
    fn new(
        spec: TrackSpec,
        participant_id: Option<String>,
        commands: &mpsc::Sender<Command>,
        closed: &Arc<Signal>,
        finished: &Arc<Signal>,
    ) -> Result<Self, Error> {
        if closed.is_closed() || finished.is_closed() {
            return Err(Error::Closed);
        }
        spec.validate()
            .map_err(|error| Error::Core(agent_core::AgentError::InvalidResource(error.into())))?;
        Ok(Self {
            spec,
            participant_id,
            commands: commands.clone(),
            closed: closed.clone(),
            finished: finished.clone(),
        })
    }

    async fn request<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<T, Error>>) -> Command,
    ) -> Result<T, Error> {
        if self.closed.is_closed() || self.finished.is_closed() {
            return Err(Error::Closed);
        }
        let (response, result) = oneshot::channel();
        self.commands
            .send(command(response))
            .await
            .map_err(|_| Error::Closed)?;
        result.await.map_err(|_| Error::Closed)?
    }

    fn selector(&self) -> Result<TrackSelector, Error> {
        Ok(TrackSelector {
            participant_external_id: self.participant_id.clone().ok_or_else(|| {
                Error::InvalidInput("local media does not identify a remote participant".into())
            })?,
            label: self.spec.label().into(),
        })
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
    pub async fn subscribe(&self, observation: VideoObservation) -> Result<Subscription, Error> {
        let selector = self.address.selector()?;
        self.address
            .request(|response| Command::VideoSubscription {
                selector,
                observation,
                response,
            })
            .await
    }
}

#[derive(Clone)]
pub struct RemoteAudioTrack {
    address: MediaAddress,
}

impl RemoteAudioTrack {
    pub async fn subscribe(
        &self,
        configuration: agent_core::ffi::AudioSubscriptionConfig,
    ) -> Result<Subscription, Error> {
        let selector = self.address.selector()?;
        self.address
            .request(|response| Command::AudioSubscription {
                selector,
                playout_delay: configuration.playout_delay.into(),
                response,
            })
            .await
    }
}

impl Participant {
    pub fn video(&self, label: String) -> Result<RemoteVideoTrack, Error> {
        Ok(RemoteVideoTrack {
            address: MediaAddress::new(
                TrackSpec::Video { label },
                Some(self.id().into()),
                &self.commands,
                &self.closed,
                &self.finished,
            )?,
        })
    }

    pub fn audio(&self, label: String) -> Result<RemoteAudioTrack, Error> {
        Ok(RemoteAudioTrack {
            address: MediaAddress::new(
                TrackSpec::Audio { label },
                Some(self.id().into()),
                &self.commands,
                &self.closed,
                &self.finished,
            )?,
        })
    }
}
