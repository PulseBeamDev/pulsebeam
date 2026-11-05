use super::*;
use agent_core::{ResourceCommand, TrackSelector, TrackSpec, VideoObservation};

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

    async fn publish(
        &self,
        source: &ProducerInput,
        configuration: agent_core::ffi::PublicationConfig,
    ) -> Result<Publication, Error> {
        self.request(|response| Command::PublishTrack {
            spec: self.spec.clone(),
            source: source.clone(),
            configuration,
            response,
        })
        .await
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
        }

        impl $name {
            pub async fn publish(
                &self,
                source: &ProducerInput,
                configuration: agent_core::ffi::PublicationConfig,
            ) -> Result<Publication, Error> {
                self.address.publish(source, configuration).await
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

impl<R, F, O> Actor<R, F, O>
where
    R: rtc::SessionRuntime,
    F: FnMut() -> Result<(R, O), Error>,
{
    pub(super) fn publish_track(
        &mut self,
        spec: TrackSpec,
        source: ProducerInput,
        configuration: agent_core::ffi::PublicationConfig,
    ) -> Result<Publication, Error> {
        let publication = self.claim_publication(spec)?;
        let owner = publication.inner.owner;
        let result = self
            .core
            .resource(ResourceCommand::SetPublicationMuted {
                owner,
                muted: !configuration.enabled,
            })
            .map_err(Error::from)
            .and_then(|_| self.bind_producer(owner, Some(source)));
        if let Err(error) = result {
            publication.inner.closed.close();
            self.retire_publications();
            self.retire_producers();
            return Err(error);
        }
        Ok(publication)
    }
}
