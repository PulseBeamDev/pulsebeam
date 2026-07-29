use super::driver::{AgentDriver, AgentError, AgentEvent, AgentStats, ParticipantId};
use super::handles::{
    DataPublisher, DataSubscriber, LocalTrack, OrderedTopicPublisher, OrderedTopicSubscriber,
    OutgoingCommand, RemoteTrack,
};
use super::mailbox;
use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Connecting,
    Connected,
    Reconnecting { reason: String },
    Closed { reason: String },
}

pub enum MediaEvent {
    LocalTrackAdded(LocalTrack),
    RemoteTrackDiscovered(pulsebeam_proto::signaling::Track),
    RemoteTrackAdded(RemoteTrack),
}

pub struct MediaEvents {
    rx: mailbox::Receiver<MediaEvent>,
}

impl MediaEvents {
    pub async fn recv(&mut self) -> Result<MediaEvent, mailbox::RecvError> {
        self.rx.recv().await
    }

    pub fn try_recv(&mut self) -> Result<MediaEvent, mailbox::TryRecvError> {
        self.rx.try_recv()
    }
}

struct AgentInner {
    participant_id: ParticipantId,
    commands: mailbox::Sender<OutgoingCommand>,
    stats: Arc<RwLock<AgentStats>>,
    connection: watch::Receiver<ConnectionState>,
    media_events: Mutex<Option<MediaEvents>>,
    runner: Mutex<Option<JoinHandle<Result<(), AgentError>>>>,
}

#[derive(Clone)]
pub struct Agent {
    inner: Arc<AgentInner>,
}

impl Agent {
    pub fn participant_id(&self) -> &ParticipantId {
        &self.inner.participant_id
    }

    pub fn stats(&self) -> AgentStats {
        self.inner
            .stats
            .read()
            .expect("agent statistics lock should not be poisoned")
            .clone()
    }

    pub fn connection_state(&self) -> watch::Receiver<ConnectionState> {
        self.inner.connection.clone()
    }

    pub async fn take_media_events(&self) -> Option<MediaEvents> {
        self.inner.media_events.lock().await.take()
    }

    pub fn topic(&self, topic: impl Into<String>) -> Result<Topic, AgentError> {
        let topic = topic.into();
        if topic.is_empty() {
            return Err(AgentError::Protocol("topic must not be empty".into()));
        }
        Ok(Topic {
            agent: self.clone(),
            name: topic,
        })
    }

    pub async fn set_subscriptions(
        &self,
        subscriptions: Vec<crate::manager::Subscription>,
    ) -> Result<(), AgentError> {
        self.inner
            .commands
            .send(OutgoingCommand::SetSubscriptions(subscriptions))
            .await
            .map_err(|_| AgentError::Closed)?;
        Ok(())
    }

    pub async fn set_playout_delay(&self, bounds: Option<(u32, u32)>) -> Result<(), AgentError> {
        self.inner
            .commands
            .send(OutgoingCommand::SetPlayoutDelay(bounds))
            .await
            .map_err(|_| AgentError::Closed)?;
        Ok(())
    }

    pub async fn close(&self) -> Result<(), AgentError> {
        let (response, closed) = tokio::sync::oneshot::channel();
        self.inner
            .commands
            .send(OutgoingCommand::Shutdown(response))
            .await
            .map_err(|_| AgentError::Closed)?;
        closed.await.map_err(|_| AgentError::Closed)?;
        if let Some(runner) = self.inner.runner.lock().await.take() {
            runner
                .await
                .map_err(|error| AgentError::Protocol(format!("agent runner failed: {error}")))??;
        }
        Ok(())
    }

    pub(crate) async fn attach_runner(&self, runner: JoinHandle<Result<(), AgentError>>) {
        let mut current = self.inner.runner.lock().await;
        debug_assert!(current.is_none());
        *current = Some(runner);
    }
}

#[derive(Clone)]
pub struct Topic {
    agent: Agent,
    name: String,
}

impl Topic {
    pub fn publisher(&self) -> PublisherBuilder {
        PublisherBuilder {
            agent: self.agent.clone(),
            topic: self.name.clone(),
        }
    }

    pub fn subscriber(&self) -> SubscriberBuilder {
        SubscriberBuilder {
            agent: self.agent.clone(),
            topic: self.name.clone(),
            publisher_id: None,
        }
    }
}

pub struct PublisherBuilder {
    agent: Agent,
    topic: String,
}

impl PublisherBuilder {
    pub fn ordered(self) -> OrderedPublisherBuilder {
        OrderedPublisherBuilder(self)
    }

    pub fn latest(self) -> LatestPublisherBuilder {
        LatestPublisherBuilder(self)
    }
}

pub struct SubscriberBuilder {
    agent: Agent,
    topic: String,
    publisher_id: Option<String>,
}

impl SubscriberBuilder {
    pub fn ordered(self) -> OrderedSubscriberBuilder {
        OrderedSubscriberBuilder(self)
    }

    pub fn latest(self) -> LatestSubscriberBuilder {
        LatestSubscriberBuilder(self)
    }
}

pub struct OrderedPublisherBuilder(PublisherBuilder);
pub struct LatestPublisherBuilder(PublisherBuilder);
pub struct OrderedSubscriberBuilder(SubscriberBuilder);
pub struct LatestSubscriberBuilder(SubscriberBuilder);

impl OrderedPublisherBuilder {
    async fn resolve(self) -> Result<OrderedTopicPublisher, AgentError> {
        let (response, result) = tokio::sync::oneshot::channel();
        self.0
            .agent
            .inner
            .commands
            .send(OutgoingCommand::DeclareOrderedPublisher {
                topic: self.0.topic,
                response,
            })
            .await
            .map_err(|_| AgentError::Closed)?;
        result.await.map_err(|_| AgentError::Closed)?
    }
}

impl LatestPublisherBuilder {
    async fn resolve(self) -> Result<DataPublisher, AgentError> {
        let (response, result) = tokio::sync::oneshot::channel();
        self.0
            .agent
            .inner
            .commands
            .send(OutgoingCommand::DeclareLatestPublisher {
                topic: self.0.topic,
                response,
            })
            .await
            .map_err(|_| AgentError::Closed)?;
        result.await.map_err(|_| AgentError::Closed)?
    }
}

impl OrderedSubscriberBuilder {
    async fn resolve(self) -> Result<OrderedTopicSubscriber, AgentError> {
        let (response, result) = tokio::sync::oneshot::channel();
        self.0
            .agent
            .inner
            .commands
            .send(OutgoingCommand::DeclareOrderedSubscriber {
                topic: self.0.topic,
                response,
            })
            .await
            .map_err(|_| AgentError::Closed)?;
        result.await.map_err(|_| AgentError::Closed)?
    }
}

impl LatestSubscriberBuilder {
    pub fn from_publisher(mut self, publisher_id: impl Into<String>) -> Self {
        let publisher_id = publisher_id.into();
        debug_assert!(!publisher_id.is_empty());
        self.0.publisher_id = Some(publisher_id);
        self
    }

    async fn resolve(self) -> Result<DataSubscriber, AgentError> {
        let (response, result) = tokio::sync::oneshot::channel();
        self.0
            .agent
            .inner
            .commands
            .send(OutgoingCommand::DeclareLatestSubscriber {
                topic: self.0.topic,
                publisher_id: self.0.publisher_id,
                response,
            })
            .await
            .map_err(|_| AgentError::Closed)?;
        result.await.map_err(|_| AgentError::Closed)?
    }
}

impl IntoFuture for OrderedPublisherBuilder {
    type Output = Result<OrderedTopicPublisher, AgentError>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.resolve())
    }
}

impl IntoFuture for LatestPublisherBuilder {
    type Output = Result<DataPublisher, AgentError>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.resolve())
    }
}

impl IntoFuture for OrderedSubscriberBuilder {
    type Output = Result<OrderedTopicSubscriber, AgentError>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.resolve())
    }
}

impl IntoFuture for LatestSubscriberBuilder {
    type Output = Result<DataSubscriber, AgentError>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.resolve())
    }
}

pub struct AgentRunner {
    driver: AgentDriver,
    stats: Arc<RwLock<AgentStats>>,
    connection: watch::Sender<ConnectionState>,
    media: mailbox::Sender<MediaEvent>,
}

impl AgentRunner {
    pub(crate) fn new(driver: AgentDriver) -> (Agent, Self) {
        let participant_id = driver.participant_id().clone();
        let commands = driver.command_sender();
        let stats = Arc::new(RwLock::new(driver.stats().clone()));
        let (connection, connection_rx) = watch::channel(ConnectionState::Connecting);
        let (media, media_rx) = mailbox::bounded(64);
        let agent = Agent {
            inner: Arc::new(AgentInner {
                participant_id,
                commands,
                stats: stats.clone(),
                connection: connection_rx,
                media_events: Mutex::new(Some(MediaEvents { rx: media_rx })),
                runner: Mutex::new(None),
            }),
        };
        (
            agent,
            Self {
                driver,
                stats,
                connection,
                media,
            },
        )
    }

    pub async fn run(mut self) -> Result<(), AgentError> {
        while let Some(event) = self.driver.poll().await {
            *self
                .stats
                .write()
                .expect("agent statistics lock should not be poisoned") =
                self.driver.stats().clone();
            match event {
                AgentEvent::StatsUpdated => {}
                AgentEvent::LocalTrackAdded(track) => {
                    self.send_media_event(MediaEvent::LocalTrackAdded(track))?;
                }
                AgentEvent::RemoteTrackDiscovered(track) => {
                    self.send_media_event(MediaEvent::RemoteTrackDiscovered(track))?;
                }
                AgentEvent::RemoteTrackAdded(track) => {
                    self.send_media_event(MediaEvent::RemoteTrackAdded(track))?;
                }
                AgentEvent::Connected => {
                    let _ = self.connection.send(ConnectionState::Connected);
                }
                AgentEvent::Disconnected(reason) => {
                    let _ = self
                        .connection
                        .send(ConnectionState::Reconnecting { reason });
                }
            }
        }
        self.driver.shutdown().await;
        for response in self.driver.take_shutdown_responses() {
            let _ = response.send(());
        }
        let _ = self.connection.send(ConnectionState::Closed {
            reason: "agent runner stopped".into(),
        });
        Ok(())
    }

    fn send_media_event(&self, event: MediaEvent) -> Result<(), AgentError> {
        self.media.try_send(event).map_err(|error| match error {
            mailbox::TrySendError::Full(_) => {
                AgentError::Protocol("media event queue capacity exceeded".into())
            }
            mailbox::TrySendError::Disconnected(_) => AgentError::Closed,
        })
    }
}
