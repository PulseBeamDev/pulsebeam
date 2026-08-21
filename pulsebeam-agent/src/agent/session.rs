use super::driver::{AgentDriver, AgentError, AgentEvent, ParticipantId, StatisticsSnapshot};
use super::handles::{
    DataPublisher, DataSubscriber, LocalEncoding, OrderedTopicPublisher, OrderedTopicSubscriber,
    OutgoingCommand, Publication, PublicationLease, RemoteTrack,
};
use super::mailbox;
use super::slots::Speaker;
use std::collections::VecDeque;
use std::collections::{HashMap, HashSet};
use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::watch;
#[cfg(feature = "sim")]
use tracing::Instrument;

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

#[derive(Clone)]
pub struct Statistics {
    state: watch::Receiver<Arc<StatisticsSnapshot>>,
}

impl Statistics {
    pub fn current(&self) -> Arc<StatisticsSnapshot> {
        self.state.borrow().clone()
    }

    pub async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        self.state.changed().await
    }
}

struct AgentInner {
    participant_id: ParticipantId,
    commands: mailbox::Sender<OutgoingCommand>,
    stats: watch::Receiver<Arc<StatisticsSnapshot>>,
    connection: watch::Receiver<ConnectionState>,
    publications: watch::Receiver<Arc<HashMap<String, Publication>>>,
    participants: watch::Receiver<Arc<HashSet<ParticipantId>>>,
    speakers: watch::Receiver<Arc<[Speaker]>>,
}

#[derive(Clone)]
pub struct Agent {
    inner: Arc<AgentInner>,
}

impl Agent {
    pub fn participant_id(&self) -> &ParticipantId {
        &self.inner.participant_id
    }

    pub fn stats(&self) -> Statistics {
        Statistics {
            state: self.inner.stats.clone(),
        }
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

    pub fn participants(&self) -> Participants {
        Participants::new(self.clone())
    }

    pub fn participant(&self, participant_id: impl Into<ParticipantId>) -> Participant {
        Participant {
            agent: self.clone(),
            id: participant_id.into(),
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

pub struct LocalTrack {
    kind: str0m::media::MediaKind,
    encodings: Vec<LocalEncoding>,
    lease: PublicationLease,
    commands: mailbox::Sender<OutgoingCommand>,
    released: bool,
}

impl LocalTrack {
    pub(crate) fn new(
        kind: str0m::media::MediaKind,
        lease: PublicationLease,
        encodings: Vec<LocalEncoding>,
        commands: mailbox::Sender<OutgoingCommand>,
    ) -> Self {
        debug_assert!(!encodings.is_empty());
        debug_assert!(encodings.iter().all(|encoding| encoding.mid == lease.mid));
        Self {
            kind,
            encodings,
            lease,
            commands,
            released: false,
        }
    }

    pub fn kind(&self) -> str0m::media::MediaKind {
        self.kind
    }

    pub fn encodings(&self) -> &[LocalEncoding] {
        &self.encodings
    }

    pub fn encodings_mut(&mut self) -> &mut [LocalEncoding] {
        &mut self.encodings
    }

    pub async fn unpublish(mut self) -> Result<(), AgentError> {
        let (response, result) = tokio::sync::oneshot::channel();
        self.commands
            .send(OutgoingCommand::Unpublish {
                lease: self.lease,
                response: Some(response),
            })
            .await
            .map_err(|_| AgentError::Closed)?;
        result.await.map_err(|_| AgentError::Closed)??;
        self.released = true;
        Ok(())
    }
}

impl Drop for LocalTrack {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        let _ = self.commands.try_send(OutgoingCommand::Unpublish {
            lease: self.lease,
            response: None,
        });
    }
}

/// The ranked list of who this receiver is hearing.
#[derive(Clone)]
pub struct Speakers {
    state: watch::Receiver<Arc<[Speaker]>>,
}

impl Speakers {
    pub fn current(&self) -> Arc<[Speaker]> {
        self.state.borrow().clone()
    }

    pub async fn changed(&mut self) -> Result<(), watch::error::RecvError> {
        self.state.changed().await
    }
}

/// Every audio track the SFU decides to forward to this receiver.
///
/// The stream never ends on its own: a slot vacated by one speaker is filled by the next, and
/// each arrival is a fresh track. Pair it with [`Media::speakers`] to know who is in which slot.
pub struct AudioTracks {
    rx: mailbox::Receiver<RemoteTrack>,
}

impl AudioTracks {
    pub async fn next(&mut self) -> Result<RemoteTrack, AgentError> {
        self.rx.recv().await.map_err(|_| AgentError::Closed)
    }
}

impl Media {
    /// Who is being heard right now, loudest first.
    ///
    /// A snapshot rather than a stream: a UI redraws from the current ranking, and a caller that
    /// wants transitions can await [`Speakers::changed`].
    pub fn speakers(&self) -> Speakers {
        Speakers {
            state: self.agent.inner.speakers.clone(),
        }
    }

    /// Register to receive audio.
    ///
    /// Unlike video there is nothing to subscribe to: the SFU forwards the loudest few speakers
    /// and decides for itself who those are, so a receiver says only that it wants audio at all.
    pub async fn receive_audio(&self) -> Result<AudioTracks, AgentError> {
        let (response, result) = tokio::sync::oneshot::channel();
        self.agent
            .inner
            .commands
            .send(OutgoingCommand::ReceiveAudio { response })
            .await
            .map_err(|_| AgentError::Closed)?;
        let rx = result.await.map_err(|_| AgentError::Closed)?;
        Ok(AudioTracks { rx })
    }

    pub async fn publish_video(&self) -> Result<LocalTrack, AgentError> {
        self.publish(str0m::media::MediaKind::Video).await
    }

    pub async fn publish_audio(&self) -> Result<LocalTrack, AgentError> {
        self.publish(str0m::media::MediaKind::Audio).await
    }

    async fn publish(&self, kind: str0m::media::MediaKind) -> Result<LocalTrack, AgentError> {
        let (response, result) = tokio::sync::oneshot::channel();
        self.agent
            .inner
            .commands
            .send(OutgoingCommand::Publish { kind, response })
            .await
            .map_err(|_| AgentError::Closed)?;
        result.await.map_err(|_| AgentError::Closed)?
    }

    pub(crate) async fn subscribe(
        &self,
        subscription: crate::manager::VideoSubscription,
    ) -> Result<RemoteTrack, AgentError> {
        let (response, result) = tokio::sync::oneshot::channel();
        self.agent
            .inner
            .commands
            .send(OutgoingCommand::SubscribeMedia {
                subscription,
                response,
            })
            .await
            .map_err(|_| AgentError::Closed)?;
        result.await.map_err(|_| AgentError::Closed)?
    }

    /// How this client wants its audio slots filled.
    ///
    /// Declarative and complete: what is sent replaces what came before, so a caller drops a pin
    /// by sending an intent without it rather than by calling an opposite. Sending nothing at all
    /// leaves the default, which is what the SFU did before a client could say anything about
    /// audio: fill every slot by loudness.
    pub async fn set_audio_intent(&self, intent: AudioIntent) -> Result<(), AgentError> {
        self.agent
            .inner
            .commands
            .send(OutgoingCommand::SetAudioIntent(intent))
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

/// A subscriber's audio selection policy.
///
/// Pins name tracks rather than participants because selection ranks tracks: one participant may
/// publish a microphone and a screen-share audio track, and pinning one must not pin the other.
/// `Participant::audio()` resolves a person to their tracks for the common case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioIntent {
    /// Tracks that hold a slot regardless of loudness, in preference order. Pins beyond the
    /// negotiated slot count are ignored rather than rejected.
    pub pinned: Vec<String>,
    /// Fill the slots pinning does not claim by loudness. `false` hears only the pins.
    pub auto: bool,
}

impl Default for AudioIntent {
    fn default() -> Self {
        Self {
            pinned: Vec::new(),
            auto: true,
        }
    }
}

impl AudioIntent {
    /// Hear only these tracks, and nothing else.
    pub fn only(pinned: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            pinned: pinned.into_iter().map(Into::into).collect(),
            auto: false,
        }
    }

    /// Hold slots for these tracks, and fill what is left by loudness.
    pub fn pinning(pinned: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            pinned: pinned.into_iter().map(Into::into).collect(),
            auto: true,
        }
    }
}

#[derive(Clone)]
pub struct Participant {
    agent: Agent,
    id: ParticipantId,
}

impl Participant {
    pub fn id(&self) -> &ParticipantId {
        &self.id
    }

    pub fn video(&self) -> RemoteVideo {
        RemoteVideo {
            participant: self.clone(),
        }
    }

    pub fn has_video(&self) -> bool {
        self.agent
            .inner
            .publications
            .borrow()
            .values()
            .any(|publication| {
                publication.publisher_id() == self.id
                    && publication.kind() == Some(str0m::media::MediaKind::Video)
            })
    }

    /// Whether the SFU has stopped forwarding this participant's video.
    ///
    /// A paused track is present and not flowing - the SFU could not afford it and shed it rather
    /// than dropping the subscription. Distinguishing that from a dead connection is what lets a
    /// UI show a placeholder instead of a blank tile, and it is not inferable from the media
    /// stream, where both look like an absence of packets.
    pub fn video_paused(&self) -> bool {
        self.agent
            .inner
            .publications
            .borrow()
            .values()
            .any(|publication| {
                publication.publisher_id() == self.id
                    && publication.kind() == Some(str0m::media::MediaKind::Video)
                    && publication.is_paused()
            })
    }

    /// The audio tracks this participant is publishing, for pinning.
    ///
    /// Usually one. A participant sharing a screen with its audio publishes two, and they are
    /// separately pinnable - which is why this returns a list rather than an option.
    pub fn audio_tracks(&self) -> Vec<String> {
        self.agent
            .inner
            .publications
            .borrow()
            .values()
            .filter(|publication| {
                publication.publisher_id() == self.id
                    && publication.kind() == Some(str0m::media::MediaKind::Audio)
            })
            .map(|publication| publication.id().to_owned())
            .collect()
    }

    pub fn has_audio(&self) -> bool {
        self.agent
            .inner
            .publications
            .borrow()
            .values()
            .any(|publication| {
                publication.publisher_id() == self.id
                    && publication.kind() == Some(str0m::media::MediaKind::Audio)
            })
    }
}

#[derive(Clone)]
pub struct RemoteVideo {
    participant: Participant,
}

impl RemoteVideo {
    pub fn subscribe(&self) -> VideoSubscriber {
        VideoSubscriber {
            participant: self.participant.clone(),
            height: 720,
            min_height: 0,
            min_fps: 0,
            priority: 0,
        }
    }
}

pub struct VideoSubscriber {
    participant: Participant,
    height: u32,
    min_height: u32,
    min_fps: u32,
    priority: u32,
}

impl VideoSubscriber {
    pub fn target_height(mut self, height: u32) -> Self {
        self.height = height;
        self
    }

    pub fn minimum_height(mut self, height: u32) -> Self {
        self.min_height = height;
        self
    }

    /// Temporal floor: keep at least this frame rate for a scalable stream.
    pub fn minimum_fps(mut self, fps: u32) -> Self {
        self.min_fps = fps;
        self
    }

    pub fn priority(mut self, priority: u32) -> Self {
        self.priority = priority;
        self
    }

    async fn resolve(self) -> Result<RemoteTrack, AgentError> {
        let mut publications = self.participant.agent.inner.publications.clone();
        loop {
            let matching: Vec<_> = publications
                .borrow()
                .values()
                .filter(|publication| {
                    publication.publisher_id() == self.participant.id
                        && publication.kind() == Some(str0m::media::MediaKind::Video)
                })
                .map(|publication| publication.id().to_owned())
                .collect();
            debug_assert!(matching.len() <= 1);
            if let Some(track_id) = matching.into_iter().next() {
                return self
                    .participant
                    .agent
                    .media()
                    .subscribe(
                        crate::manager::VideoSubscription::new(track_id)
                            .target_height(self.height)
                            .minimum_height(self.min_height)
                            .minimum_fps(self.min_fps)
                            .priority(self.priority),
                    )
                    .await;
            }
            publications
                .changed()
                .await
                .map_err(|_| AgentError::Closed)?;
        }
    }
}

impl IntoFuture for VideoSubscriber {
    type Output = Result<RemoteTrack, AgentError>;
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.resolve())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ParticipantAvailability {
    video: bool,
    audio: bool,
    /// Whether the SFU has stopped forwarding the video. Part of availability rather than a detail
    /// of it: a paused track is present and not flowing, and a change feed that omitted it left
    /// applications redrawing nothing while the picture stopped.
    video_paused: bool,
}

pub enum ParticipantChange {
    Joined(Participant),
    Updated(Participant),
    Left(ParticipantId),
}

pub struct Participants {
    agent: Agent,
    publications: watch::Receiver<Arc<HashMap<String, Publication>>>,
    participants: watch::Receiver<Arc<HashSet<ParticipantId>>>,
    known: HashMap<ParticipantId, ParticipantAvailability>,
    pending: VecDeque<ParticipantChange>,
}

impl Participants {
    fn new(agent: Agent) -> Self {
        let publications = agent.inner.publications.clone();
        let participants = agent.inner.participants.clone();
        let known = participant_availability(&participants.borrow(), &publications.borrow());
        let pending = known
            .keys()
            .cloned()
            .map(|id| ParticipantChange::Joined(agent.participant(id)))
            .collect();
        Self {
            agent,
            publications,
            participants,
            known,
            pending,
        }
    }

    pub async fn next(&mut self) -> Result<ParticipantChange, AgentError> {
        loop {
            if let Some(change) = self.pending.pop_front() {
                return Ok(change);
            }
            tokio::select! {
                result = self.publications.changed() => result.map_err(|_| AgentError::Closed)?,
                result = self.participants.changed() => result.map_err(|_| AgentError::Closed)?,
            }
            let current =
                participant_availability(&self.participants.borrow(), &self.publications.borrow());
            for (id, availability) in &current {
                match self.known.get(id) {
                    None => self.pending.push_back(ParticipantChange::Joined(
                        self.agent.participant(id.clone()),
                    )),
                    Some(previous) if previous != availability => self.pending.push_back(
                        ParticipantChange::Updated(self.agent.participant(id.clone())),
                    ),
                    Some(_) => {}
                }
            }
            for id in self.known.keys() {
                if !current.contains_key(id) {
                    self.pending.push_back(ParticipantChange::Left(id.clone()));
                }
            }
            self.known = current;
        }
    }

    pub async fn joined(&mut self) -> Result<Participant, AgentError> {
        loop {
            if let ParticipantChange::Joined(participant) = self.next().await? {
                return Ok(participant);
            }
        }
    }
}

fn participant_availability(
    roster: &HashSet<ParticipantId>,
    publications: &HashMap<String, Publication>,
) -> HashMap<ParticipantId, ParticipantAvailability> {
    let mut participants = HashMap::new();
    for participant in roster {
        participants.insert(
            participant.clone(),
            ParticipantAvailability {
                video: false,
                audio: false,
                video_paused: false,
            },
        );
    }
    for publication in publications.values() {
        if !participants.contains_key(publication.publisher_id()) {
            continue;
        }
        let availability = participants
            .entry(publication.publisher_id().to_owned())
            .or_insert(ParticipantAvailability {
                video: false,
                audio: false,
                video_paused: false,
            });
        match publication.kind() {
            Some(str0m::media::MediaKind::Video) => {
                availability.video = true;
                // Part of availability, not merely a detail of it: a paused track is present and
                // not flowing, and a change feed that omits it leaves an application redrawing
                // nothing while the picture stops.
                availability.video_paused |= publication.is_paused();
            }
            Some(str0m::media::MediaKind::Audio) => availability.audio = true,
            None => {}
        }
    }
    participants
}

#[cfg(test)]
mod tests {
    use super::*;
    use pulsebeam_proto::signaling::Publication as Track;

    #[test]
    fn publications_collapse_into_participant_availability() {
        let publications = [
            (
                "video".to_owned(),
                Publication::from_signaling(Track {
                    track_id: "video".into(),
                    kind: 1,
                    participant_id: "alice".into(),
                }),
            ),
            (
                "audio".to_owned(),
                Publication::from_signaling(Track {
                    track_id: "audio".into(),
                    kind: 2,
                    participant_id: "alice".into(),
                }),
            ),
        ]
        .into_iter()
        .collect();

        let participants =
            participant_availability(&["alice".to_owned()].into_iter().collect(), &publications);

        assert_eq!(participants.len(), 1);
        assert_eq!(
            participants["alice"],
            ParticipantAvailability {
                video: true,
                audio: true,
                video_paused: false,
            }
        );
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
        Ok(())
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
    stats: watch::Sender<Arc<StatisticsSnapshot>>,
    connection: watch::Sender<ConnectionState>,
    publications: watch::Sender<Arc<HashMap<String, Publication>>>,
    participants: watch::Sender<Arc<HashSet<ParticipantId>>>,
    speakers: watch::Sender<Arc<[Speaker]>>,
    publication_state: HashMap<String, Publication>,
    participant_state: HashSet<ParticipantId>,
    /// Correlates agent-side str0m logs with the SFU peer in simulator traces.
    #[cfg(feature = "sim")]
    sim_span: tracing::Span,
}

impl AgentRunner {
    pub(crate) fn new(driver: AgentDriver) -> (Agent, Self) {
        let participant_id = driver.participant_id().clone();
        #[cfg(feature = "sim")]
        let sim_span = tracing::info_span!(
            "peer",
            participant_id = %participant_id,
            ext_room_id = %driver.room_id()
        );
        let commands = driver.command_sender();
        let (stats, stats_rx) = watch::channel(Arc::new(driver.stats().clone()));
        let (connection, connection_rx) = watch::channel(ConnectionState::Connecting);
        let (publications, publication_rx) = watch::channel(Arc::new(HashMap::new()));
        let (participants, participants_rx) = watch::channel(Arc::new(HashSet::new()));
        let (speakers, speakers_rx) = watch::channel(Arc::from(Vec::new()));
        let agent = Agent {
            inner: Arc::new(AgentInner {
                participant_id,
                commands,
                stats: stats_rx,
                connection: connection_rx,
                publications: publication_rx,
                participants: participants_rx,
                speakers: speakers_rx,
            }),
        };
        (
            agent,
            Self {
                driver,
                stats,
                connection,
                publications,
                participants,
                speakers,
                publication_state: HashMap::new(),
                participant_state: HashSet::new(),
                #[cfg(feature = "sim")]
                sim_span,
            },
        )
    }

    pub async fn run(mut self) -> Result<(), AgentError> {
        while let Some(event) = {
            #[cfg(feature = "sim")]
            {
                self.driver.poll().instrument(self.sim_span.clone()).await
            }
            #[cfg(not(feature = "sim"))]
            {
                self.driver.poll().await
            }
        } {
            match event {
                AgentEvent::StatsUpdated => {
                    self.stats
                        .send_replace(Arc::new(self.driver.stats().clone()));
                }
                AgentEvent::ParticipantsChanged {
                    added,
                    removed,
                    snapshot,
                } => {
                    if snapshot {
                        self.participant_state = added.into_iter().collect();
                    } else {
                        self.participant_state.extend(added);
                        for participant in removed {
                            self.participant_state.remove(&participant);
                        }
                    }
                    self.participants
                        .send_replace(Arc::new(self.participant_state.clone()));
                }
                AgentEvent::RemoteTrackDiscovered(track) => {
                    let publication = Publication::from_signaling(track);
                    self.publication_state
                        .insert(publication.id().to_owned(), publication);
                    self.publish_publications();
                }
                AgentEvent::SpeakersChanged(speakers) => {
                    self.speakers.send_replace(Arc::from(speakers));
                }
                AgentEvent::RemoteTrackRemoved(track_id) => {
                    self.publication_state.remove(&track_id);
                    self.publish_publications();
                }
                AgentEvent::RemoteTrackPaused(track_id) => {
                    if let Some(p) = self.publication_state.get_mut(&track_id) {
                        p.set_paused(true);
                        self.publish_publications();
                    }
                }
                AgentEvent::RemoteTrackResumed(track_id) => {
                    if let Some(p) = self.publication_state.get_mut(&track_id) {
                        p.set_paused(false);
                        self.publish_publications();
                    }
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

    fn publish_publications(&self) {
        let _ = self
            .publications
            .send(Arc::new(self.publication_state.clone()));
    }
}
