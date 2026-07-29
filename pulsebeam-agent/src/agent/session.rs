use super::driver::{AgentDriver, AgentError, AgentEvent, AgentStats, ParticipantId};
use super::handles::{
    DataPublisher, DataSubscriber, LocalTrack, OrderedTopicPublisher, OrderedTopicSubscriber,
    OutgoingCommand, RemoteTrack,
};
use super::mailbox;
use std::collections::HashMap;
use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex, RwLock};
use tokio::sync::{Mutex, Notify, watch};
use tokio::task::JoinHandle;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionState {
    Connecting,
    Connected,
    Reconnecting { reason: String },
    Closed { reason: String },
}

#[derive(Clone)]
pub struct Connection {
    state: watch::Receiver<ConnectionState>,
}

impl Connection {
    pub fn current(&self) -> ConnectionState {
        self.state.borrow().clone()
    }

    pub async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        self.state.changed().await
    }
}

pub enum MediaEvent {
    RemoteTrackAdded(RemoteTrack),
}

#[derive(Clone)]
pub struct Publications {
    state: watch::Receiver<Arc<HashMap<String, pulsebeam_proto::signaling::Track>>>,
}

impl Publications {
    pub fn current(&self) -> Arc<HashMap<String, pulsebeam_proto::signaling::Track>> {
        self.state.borrow().clone()
    }

    pub async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        self.state.changed().await
    }
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
    local_tracks: Arc<StdMutex<Vec<LocalTrack>>>,
    local_tracks_changed: Arc<Notify>,
    publications: watch::Receiver<Arc<HashMap<String, pulsebeam_proto::signaling::Track>>>,
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

    pub fn connection(&self) -> Connection {
        Connection {
            state: self.inner.connection.clone(),
        }
    }

    pub async fn closed(&self) -> ConnectionState {
        let mut connection = self.connection();
        loop {
            let state = connection.current();
            if matches!(state, ConnectionState::Closed { .. }) {
                return state;
            }
            if connection.changed().await.is_err() {
                return ConnectionState::Closed {
                    reason: "agent runner stopped".into(),
                };
            }
        }
    }

    pub fn media(&self) -> Media {
        Media {
            agent: self.clone(),
        }
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
}

#[derive(Clone)]
pub struct Media {
    agent: Agent,
}

pub struct LocalPublication {
    name: String,
    kind: str0m::media::MediaKind,
    tracks: Option<Vec<LocalTrack>>,
    commands: mailbox::Sender<OutgoingCommand>,
    available: Arc<StdMutex<Vec<LocalTrack>>>,
    available_changed: Arc<Notify>,
}

impl LocalPublication {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn kind(&self) -> str0m::media::MediaKind {
        self.kind
    }

    pub fn tracks(&self) -> &[LocalTrack] {
        self.tracks
            .as_deref()
            .expect("released publication cannot expose tracks")
    }

    pub fn tracks_mut(&mut self) -> &mut [LocalTrack] {
        self.tracks
            .as_deref_mut()
            .expect("released publication cannot expose tracks")
    }

    pub async fn unpublish(mut self) -> Result<(), AgentError> {
        self.deactivate().await?;
        self.release();
        Ok(())
    }

    async fn deactivate(&self) -> Result<(), AgentError> {
        let mid = self
            .tracks()
            .first()
            .expect("publication must contain a track")
            .mid;
        self.commands
            .send(OutgoingCommand::SetUpstreamActive { mid, active: false })
            .await
            .map_err(|_| AgentError::Closed)
    }

    fn release(&mut self) {
        let Some(mut tracks) = self.tracks.take() else {
            return;
        };
        debug_assert!(!tracks.is_empty());
        self.available
            .lock()
            .expect("local track lock should not be poisoned")
            .append(&mut tracks);
        self.available_changed.notify_waiters();
    }
}

impl Drop for LocalPublication {
    fn drop(&mut self) {
        let Some(mid) = self
            .tracks
            .as_ref()
            .and_then(|tracks| tracks.first())
            .map(|track| track.mid)
        else {
            return;
        };
        let _ = self
            .commands
            .try_send(OutgoingCommand::SetUpstreamActive { mid, active: false });
        self.release();
    }
}

impl Media {
    pub fn publications(&self) -> Publications {
        Publications {
            state: self.agent.inner.publications.clone(),
        }
    }

    pub async fn publish_video(
        &self,
        name: impl Into<String>,
    ) -> Result<LocalPublication, AgentError> {
        self.publish(name.into(), str0m::media::MediaKind::Video)
            .await
    }

    pub async fn publish_audio(
        &self,
        name: impl Into<String>,
    ) -> Result<LocalPublication, AgentError> {
        self.publish(name.into(), str0m::media::MediaKind::Audio)
            .await
    }

    async fn publish(
        &self,
        name: String,
        kind: str0m::media::MediaKind,
    ) -> Result<LocalPublication, AgentError> {
        if name.is_empty() {
            return Err(AgentError::Protocol(
                "publication name must not be empty".into(),
            ));
        }
        let mut connection = self.agent.connection();
        loop {
            let notified = self.agent.inner.local_tracks_changed.notified();
            let available = {
                let mut tracks = self
                    .agent
                    .inner
                    .local_tracks
                    .lock()
                    .expect("local track lock should not be poisoned");
                if let Some(mid) = tracks
                    .iter()
                    .find(|track| track.kind == kind)
                    .map(|track| track.mid)
                {
                    let mut publication_tracks = Vec::new();
                    let mut index = 0;
                    while index < tracks.len() {
                        if tracks[index].mid == mid {
                            publication_tracks.push(tracks.swap_remove(index));
                        } else {
                            index += 1;
                        }
                    }
                    debug_assert!(!publication_tracks.is_empty());
                    Some((mid, publication_tracks))
                } else {
                    None
                }
            };
            if let Some((mid, mut publication_tracks)) = available {
                let result = self
                    .agent
                    .inner
                    .commands
                    .send(OutgoingCommand::SetUpstreamActive { mid, active: true })
                    .await;
                if result.is_err() {
                    self.agent
                        .inner
                        .local_tracks
                        .lock()
                        .expect("local track lock should not be poisoned")
                        .append(&mut publication_tracks);
                    return Err(AgentError::Closed);
                }
                return Ok(LocalPublication {
                    name,
                    kind,
                    tracks: Some(publication_tracks),
                    commands: self.agent.inner.commands.clone(),
                    available: self.agent.inner.local_tracks.clone(),
                    available_changed: self.agent.inner.local_tracks_changed.clone(),
                });
            }

            match connection.current() {
                ConnectionState::Connected => return Err(AgentError::MediaCapacity(kind)),
                ConnectionState::Closed { .. } => return Err(AgentError::Closed),
                ConnectionState::Connecting | ConnectionState::Reconnecting { .. } => {}
            }

            tokio::select! {
                _ = notified => {}
                result = connection.changed() => {
                    result.map_err(|_| AgentError::Closed)?;
                }
            }
        }
    }

    pub async fn events(&self) -> Result<MediaEvents, AgentError> {
        self.agent
            .inner
            .media_events
            .lock()
            .await
            .take()
            .ok_or_else(|| AgentError::Protocol("media events are already being observed".into()))
    }

    pub async fn set_view(
        &self,
        subscriptions: Vec<crate::manager::VideoSubscription>,
    ) -> Result<(), AgentError> {
        self.agent
            .inner
            .commands
            .send(OutgoingCommand::SetSubscriptions(subscriptions))
            .await
            .map_err(|_| AgentError::Closed)?;
        Ok(())
    }

    pub async fn set_playout_delay(&self, bounds: Option<(u32, u32)>) -> Result<(), AgentError> {
        self.agent
            .inner
            .commands
            .send(OutgoingCommand::SetPlayoutDelay(bounds))
            .await
            .map_err(|_| AgentError::Closed)?;
        Ok(())
    }
}

impl Agent {
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
    publications: watch::Sender<Arc<HashMap<String, pulsebeam_proto::signaling::Track>>>,
    publication_state: HashMap<String, pulsebeam_proto::signaling::Track>,
    local_tracks: Arc<StdMutex<Vec<LocalTrack>>>,
    local_tracks_changed: Arc<Notify>,
}

impl AgentRunner {
    pub(crate) fn new(driver: AgentDriver) -> (Agent, Self) {
        let participant_id = driver.participant_id().clone();
        let commands = driver.command_sender();
        let stats = Arc::new(RwLock::new(driver.stats().clone()));
        let (connection, connection_rx) = watch::channel(ConnectionState::Connecting);
        let (media, media_rx) = mailbox::bounded(64);
        let (publications, publication_rx) = watch::channel(Arc::new(HashMap::new()));
        let local_tracks = Arc::new(StdMutex::new(Vec::new()));
        let local_tracks_changed = Arc::new(Notify::new());
        let agent = Agent {
            inner: Arc::new(AgentInner {
                participant_id,
                commands,
                stats: stats.clone(),
                connection: connection_rx,
                media_events: Mutex::new(Some(MediaEvents { rx: media_rx })),
                local_tracks: local_tracks.clone(),
                local_tracks_changed: local_tracks_changed.clone(),
                publications: publication_rx,
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
                publications,
                publication_state: HashMap::new(),
                local_tracks,
                local_tracks_changed,
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
                    self.local_tracks
                        .lock()
                        .expect("local track lock should not be poisoned")
                        .push(track);
                    self.local_tracks_changed.notify_waiters();
                }
                AgentEvent::RemoteTrackDiscovered(track) => {
                    self.publication_state.insert(track.id.clone(), track);
                    self.publish_publications();
                }
                AgentEvent::RemoteTrackRemoved(track_id) => {
                    self.publication_state.remove(&track_id);
                    self.publish_publications();
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

    fn publish_publications(&self) {
        let _ = self
            .publications
            .send(Arc::new(self.publication_state.clone()));
    }
}
