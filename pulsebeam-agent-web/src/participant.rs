use std::collections::{BTreeMap, VecDeque};

use pulsebeam_agent_core::{
    AgentCore, AudioIntent, CoreConfig, CoreError, CoreEvent, CoreInput, HttpMethod, HttpRequest,
    IntentError, IntentState, LatencyLock, LayerOption, MonotonicTime, PlayoutPreset, SessionError,
    SessionEvent, SessionReducer, SessionSnapshot, StickyAllocation, StickyAllocator, TrackId,
    TransportGeneration, VideoIntent,
};

use crate::http::FetchClient;
use crate::interop::{
    BrowserEvent, DataChannelConfig, GenerationEvent, PeerConfig, SenderPreset, WebError,
};
use crate::topics::{TopicAction, TopicEvent, TopicRegistry};
use crate::transport::WebTransport;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParticipantEvent {
    Core(CoreEvent),
    Browser(GenerationEvent<BrowserEvent>),
    Session(SessionEvent),
    Topic(TopicEvent),
    RemoteTrack {
        generation: TransportGeneration,
        mid: String,
        kind: pulsebeam_agent_core::MediaKind,
    },
    Error(WebError),
}

struct SessionState {
    reducer: SessionReducer,
    intents: IntentState,
    latency: LatencyLock,
    allocator: StickyAllocator,
}

impl SessionState {
    fn new() -> Self {
        Self {
            reducer: SessionReducer::new(),
            intents: IntentState::default(),
            latency: LatencyLock::default(),
            allocator: StickyAllocator::new(),
        }
    }
}

pub struct WebParticipant {
    core: AgentCore,
    transport: WebTransport,
    session: SessionState,
    topics: TopicRegistry,
    fetch: FetchClient,
    events: VecDeque<ParticipantEvent>,
}

impl WebParticipant {
    pub fn new(config: CoreConfig, peer: PeerConfig) -> Result<Self, WebError> {
        let mut transport = WebTransport::new(peer)?;
        transport.register_channel(crate::interop::DataChannelConfig::reliable(
            crate::interop::SIGNALING_LABEL,
        ));
        Ok(Self {
            core: AgentCore::new(config),
            transport,
            session: SessionState::new(),
            topics: TopicRegistry::new(),
            fetch: browser_or_mock_fetch(),
            events: VecDeque::new(),
        })
    }

    pub fn core(&self) -> &AgentCore {
        &self.core
    }

    pub fn transport(&self) -> &WebTransport {
        &self.transport
    }

    pub fn transport_mut(&mut self) -> &mut WebTransport {
        &mut self.transport
    }

    pub fn session(&self) -> SessionSnapshot {
        self.session.reducer.snapshot()
    }

    pub fn topics(&self) -> &TopicRegistry {
        &self.topics
    }

    pub fn topics_mut(&mut self) -> &mut TopicRegistry {
        &mut self.topics
    }

    pub fn fetch_mut(&mut self) -> &mut FetchClient {
        &mut self.fetch
    }

    pub fn register_channel(&mut self, config: DataChannelConfig) {
        self.transport.register_channel(config);
    }

    pub fn register_latest_publisher(&mut self, topic: impl Into<String>) -> DataChannelConfig {
        let config = self.topics.register_latest_publisher(topic);
        self.transport.register_channel(config.clone());
        config
    }

    pub fn register_latest_subscriber(
        &mut self,
        topic: impl Into<String>,
        publisher_id: Option<&str>,
    ) -> DataChannelConfig {
        let config = self.topics.register_latest_subscriber(topic, publisher_id);
        self.transport.register_channel(config.clone());
        config
    }

    pub fn register_ordered_publisher(
        &mut self,
        topic: impl Into<String>,
        publisher_id: impl Into<String>,
        stream_id: u64,
    ) -> Result<DataChannelConfig, WebError> {
        let config = self
            .topics
            .register_ordered_publisher(topic, publisher_id, stream_id)
            .map_err(topic_error)?;
        self.transport.register_channel(config.clone());
        Ok(config)
    }

    pub fn register_ordered_subscriber(&mut self, topic: impl Into<String>) -> DataChannelConfig {
        let config = self.topics.register_ordered_subscriber(topic);
        self.transport.register_channel(config.clone());
        config
    }

    pub async fn apply_sender_preset(
        &mut self,
        sender: impl Into<String>,
        preset: SenderPreset,
    ) -> Result<(), WebError> {
        self.transport.queue_sender_update(sender, preset);
        self.transport.flush_sender_updates().await
    }

    pub async fn replace_sender_track(
        &mut self,
        sender: &str,
        track: Option<&crate::transport::MediaStreamTrackHandle>,
    ) -> Result<(), WebError> {
        self.transport.replace_sender_track(sender, track).await
    }

    pub async fn handle(&mut self, now: MonotonicTime, input: CoreInput) -> Result<(), WebError> {
        self.core.handle(now, input).map_err(core_error)?;
        self.drive().await
    }

    pub async fn connect(&mut self, now: MonotonicTime, uri: &str) -> Result<(), WebError> {
        self.handle(now, CoreInput::Start).await?;
        let generation = self.core.generation();
        let offer = self.transport.create_offer(generation).await?;
        let request = HttpRequest::new(HttpMethod::Post, uri, offer.into_bytes())
            .with_header("Content-Type", "application/sdp");
        let response = self
            .fetch
            .execute(request)
            .await?
            .require_success()
            .map_err(|error| WebError::Http(error.to_string()))?;
        let answer = String::from_utf8(response.body)
            .map_err(|_| WebError::Http("answer was not UTF-8 SDP".to_owned()))?;
        self.transport.set_answer(generation, &answer).await
    }

    pub async fn handle_browser_event(
        &mut self,
        now: MonotonicTime,
        event: GenerationEvent<BrowserEvent>,
    ) -> Result<(), WebError> {
        self.require_generation(event.generation)?;
        self.events
            .push_back(ParticipantEvent::Browser(event.clone()));
        match event.value {
            BrowserEvent::Connected => {
                self.handle(
                    now,
                    CoreInput::TransportConnected {
                        generation: event.generation,
                    },
                )
                .await?;
            }
            BrowserEvent::Failed(reason) => {
                self.handle(
                    now,
                    CoreInput::TransportFailed {
                        generation: event.generation,
                        reason,
                    },
                )
                .await?;
            }
            BrowserEvent::Closed => {
                self.handle(
                    now,
                    CoreInput::TransportClosed {
                        generation: event.generation,
                    },
                )
                .await?;
            }
            BrowserEvent::Signaling(bytes) => {
                self.apply_signaling(&bytes)?;
            }
            BrowserEvent::Data { label, payload } => {
                let (events, actions) =
                    self.topics.receive(&label, &payload).map_err(topic_error)?;
                for event in events {
                    self.events.push_back(ParticipantEvent::Topic(event));
                }
                self.send_topic_actions(event.generation, actions).await?;
            }
            BrowserEvent::RemoteTrack { mid, kind } => {
                self.events.push_back(ParticipantEvent::RemoteTrack {
                    generation: event.generation,
                    mid,
                    kind,
                });
            }
            BrowserEvent::Timer => self.handle(now, CoreInput::Timer).await?,
        }
        Ok(())
    }

    pub fn apply_signaling(&mut self, bytes: &[u8]) -> Result<Vec<SessionEvent>, WebError> {
        let events = self
            .session
            .reducer
            .apply_message(bytes)
            .map_err(session_error)?;
        for event in &events {
            self.events
                .push_back(ParticipantEvent::Session(event.clone()));
        }
        Ok(events)
    }

    pub async fn send(
        &mut self,
        now: MonotonicTime,
        generation: TransportGeneration,
        request_id: pulsebeam_agent_core::RequestId,
        channel: impl Into<pulsebeam_agent_core::ChannelKey>,
        payload: Vec<u8>,
    ) -> Result<(), WebError> {
        self.handle(
            now,
            CoreInput::Send {
                generation,
                request_id,
                channel: channel.into(),
                payload,
            },
        )
        .await
    }

    pub fn set_video_intent(&mut self, intent: VideoIntent) {
        self.session.intents.set_video(intent);
    }

    pub fn set_audio_intent(&mut self, intent: AudioIntent) {
        self.session.intents.set_audio(intent);
    }

    pub fn set_publish_intent(
        &mut self,
        mid: impl Into<String>,
        active: bool,
    ) -> Result<(), IntentError> {
        self.session.intents.set_publish(mid, active)
    }

    pub fn set_playout_preset(&mut self, preset: PlayoutPreset) -> Result<(), IntentError> {
        self.session
            .latency
            .apply(preset)
            .map_err(IntentError::from)
    }

    pub fn allocate(
        &mut self,
        layers: &BTreeMap<TrackId, Vec<LayerOption>>,
        budget_bps: u64,
    ) -> Result<Vec<StickyAllocation>, IntentError> {
        let intents = self.session.intents.video().cloned().collect::<Vec<_>>();
        self.session
            .allocator
            .allocate(&intents, layers, budget_bps)
    }

    pub fn poll_event(&mut self) -> Option<ParticipantEvent> {
        self.events.pop_front()
    }

    async fn drive(&mut self) -> Result<(), WebError> {
        while let Some(event) = self.core.poll_event() {
            self.events.push_back(ParticipantEvent::Core(event));
        }
        while let Some(effect) = self.core.poll_effect() {
            self.transport.execute(effect).await?;
        }
        while let Some(event) = self.transport.poll_event() {
            self.events.push_back(ParticipantEvent::Browser(event));
        }
        Ok(())
    }

    async fn send_topic_actions(
        &mut self,
        generation: TransportGeneration,
        actions: Vec<TopicAction>,
    ) -> Result<(), WebError> {
        for action in actions {
            self.transport
                .send(generation, &action.channel, action.payload)?;
        }
        Ok(())
    }

    fn require_generation(&self, received: TransportGeneration) -> Result<(), WebError> {
        let expected = self.core.generation();
        if received != expected {
            debug_assert_ne!(received, expected);
            return Err(WebError::StaleGeneration { expected, received });
        }
        Ok(())
    }
}

fn core_error(error: CoreError) -> WebError {
    WebError::Core(error.to_string())
}

fn session_error(error: SessionError) -> WebError {
    WebError::Core(error.to_string())
}

fn topic_error(error: pulsebeam_agent_core::topic::TopicError) -> WebError {
    WebError::Topic(error.to_string())
}

#[cfg(target_arch = "wasm32")]
fn browser_or_mock_fetch() -> FetchClient {
    FetchClient::browser()
}

#[cfg(not(target_arch = "wasm32"))]
fn browser_or_mock_fetch() -> FetchClient {
    FetchClient::mock()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::interop::{BrowserEvent, GenerationEvent};

    #[test]
    fn stale_browser_events_do_not_change_core_state() {
        let mut participant =
            WebParticipant::new(CoreConfig::default(), PeerConfig::default()).unwrap();
        futures_lite::future::block_on(participant.handle(MonotonicTime::ZERO, CoreInput::Start))
            .unwrap();
        let browser_event =
            GenerationEvent::new(TransportGeneration::INITIAL, BrowserEvent::Connected);
        let result = futures_lite::future::block_on(
            participant.handle_browser_event(MonotonicTime::ZERO, browser_event),
        );
        assert!(matches!(result, Err(WebError::StaleGeneration { .. })));
        assert_eq!(
            participant.core().state(),
            pulsebeam_agent_core::ConnectionState::Connecting
        );
    }

    #[test]
    fn connect_executes_core_effect_and_keeps_generation() {
        let mut participant =
            WebParticipant::new(CoreConfig::default(), PeerConfig::default()).unwrap();
        participant.fetch_mut().mock_mut().push_response(Ok(
            pulsebeam_agent_core::HttpResponse::new(200, b"answer".to_vec()),
        ));
        futures_lite::future::block_on(participant.connect(MonotonicTime::ZERO, "/offer")).unwrap();
        assert_eq!(participant.core().generation(), TransportGeneration::new(1));
        assert_eq!(
            participant.transport().generation(),
            Some(TransportGeneration::new(1))
        );
    }
}
