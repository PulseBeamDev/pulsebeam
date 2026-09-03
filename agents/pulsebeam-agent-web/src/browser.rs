use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::{Rc, Weak},
};

use agent_core::{
    AgentConfig, AudioSubscription, ChannelId, ConnectionState, DataChannelBinding,
    DataChannelEffect, DataChannelEvent, DataChannelReliability, DataChannelSpec, DesiredState, Effect,
    Failure, FailureClass, Generation, HostEvent, HttpEffect, HttpEvent, HttpHeader, HttpMethod,
    HttpResponse, MediaKind, MediaSlot, MediaTopology, Notification, OfferResources, OperationId,
    PlayoutDelay, PublicationIntent, RetryPolicy, RtcEffect, RtcEvent, SlotBinding, TimerEffect,
    TimerEvent, TopicChannel, TopicDropReason, TopicMessage, TopicMode, TopicNotification,
    TopicPublisher, TopicRegistrations, TopicSend, TopicSubscriber, VideoSubscription,
};
use js_sys::{Array, Function, Object, Reflect, Uint8Array};
use serde::Deserialize;
use wasm_bindgen::{JsCast, JsValue, closure::Closure, prelude::wasm_bindgen};
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{
    AbortController, Event, Headers, MediaStreamTrack, MessageEvent, Request, RequestInit,
    Response, RtcBundlePolicy, RtcConfiguration, RtcDataChannel, RtcDataChannelInit,
    RtcDataChannelType, RtcPeerConnection, RtcPeerConnectionState, RtcRtpSender, RtcRtpTransceiver,
    RtcRtpTransceiverDirection, RtcRtpTransceiverInit, RtcSdpType, RtcSessionDescriptionInit,
    RtcTrackEvent,
};

use crate::engine::{spawn_actor, ActorHandle, Host, PublicCommand, TopicCommand, Turn};

const SIGNALING_LABEL: &str = "v1/sys/signaling";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RuntimeConfig {
    endpoint: String,
    room_id: String,
    #[serde(default)]
    request_headers: BTreeMap<String, String>,
    topology: TopologyConfig,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TopologyConfig {
    #[serde(default)]
    local_video: Vec<String>,
    #[serde(default)]
    local_audio: Vec<String>,
    #[serde(default)]
    remote_video: u8,
    #[serde(default)]
    remote_audio: u8,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TopicRegistrationConfig {
    name: String,
    mode: TopicModeConfig,
    #[serde(default)]
    publish: bool,
    #[serde(default)]
    subscribe: bool,
    publisher_id: Option<String>,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum TopicModeConfig {
    Latest,
    Ordered,
}

impl From<TopicModeConfig> for TopicMode {
    fn from(value: TopicModeConfig) -> Self {
        match value {
            TopicModeConfig::Latest => Self::Latest,
            TopicModeConfig::Ordered => Self::Ordered,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DesiredConfig {
    connected: bool,
    #[serde(default)]
    publications: Vec<PublicationConfig>,
    #[serde(default)]
    video: Vec<VideoSubscriptionConfig>,
    #[serde(default)]
    audio: AudioSubscriptionConfig,
    #[serde(default)]
    playout_delay: PlayoutDelayConfig,
    #[serde(default)]
    topics: Vec<TopicRegistrationConfig>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PublicationConfig {
    slot: String,
    active: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VideoSubscriptionConfig {
    slot: u8,
    track_id: String,
    height: u32,
    min_height: u32,
    min_fps: u32,
    priority: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AudioSubscriptionConfig {
    #[serde(default)]
    pinned: Vec<String>,
    #[serde(default = "default_true")]
    automatic: bool,
}

impl Default for AudioSubscriptionConfig {
    fn default() -> Self {
        Self {
            pinned: Vec::new(),
            automatic: true,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase", deny_unknown_fields)]
enum PlayoutDelayConfig {
    #[default]
    Adaptive,
    Fixed {
        #[serde(rename = "minMs")]
        min_ms: u32,
        #[serde(rename = "maxMs")]
        max_ms: u32,
    },
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SenderConfig {
    content_hint: String,
    degradation_preference: Option<String>,
    #[serde(default)]
    encodings: Vec<EncodingConfig>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EncodingConfig {
    rid: Option<String>,
    active: bool,
    scale_resolution_down_by: Option<f64>,
    max_bitrate: Option<f64>,
    max_framerate: Option<f64>,
    scalability_mode: Option<String>,
    dtx: Option<String>,
}

#[derive(Clone)]
struct LocalTrackState {
    track: MediaStreamTrack,
    config: SenderConfig,
    muted: bool,
}

impl DesiredConfig {
    fn into_core(self) -> DesiredState {
        DesiredState {
            revision: 0,
            connected: self.connected,
            publications: self
                .publications
                .into_iter()
                .map(|publication| PublicationIntent {
                    slot: publication.slot,
                    active: publication.active,
                })
                .collect(),
            video: self
                .video
                .into_iter()
                .map(|video| VideoSubscription {
                    slot: video.slot,
                    track_id: video.track_id,
                    height: video.height,
                    min_height: video.min_height,
                    min_fps: video.min_fps,
                    priority: video.priority,
                })
                .collect(),
            audio: AudioSubscription {
                pinned: self.audio.pinned,
                automatic: self.audio.automatic,
            },
            playout_delay: match self.playout_delay {
                PlayoutDelayConfig::Adaptive => PlayoutDelay::Adaptive,
                PlayoutDelayConfig::Fixed { min_ms, max_ms } => {
                    PlayoutDelay::Fixed { min_ms, max_ms }
                }
            },
            topics: topic_registrations(&self.topics),
        }
    }
}

fn default_true() -> bool {
    true
}

struct DataChannel {
    channel: RtcDataChannel,
    _open: Closure<dyn FnMut(Event)>,
    _close: Closure<dyn FnMut(Event)>,
    _message: Closure<dyn FnMut(MessageEvent)>,
}

impl DataChannel {
    fn close(self) {
        self.channel.set_onopen(None);
        self.channel.set_onclose(None);
        self.channel.set_onmessage(None);
        self.channel.close();
    }
}

struct Peer {
    connection: RtcPeerConnection,
    transceivers: Vec<(MediaSlot, RtcRtpTransceiver)>,
    remote_tracks: BTreeMap<String, MediaStreamTrack>,
    channels: BTreeMap<u64, DataChannel>,
    _state: Closure<dyn FnMut(Event)>,
    _track: Closure<dyn FnMut(RtcTrackEvent)>,
}

impl Peer {
    fn close(self) {
        self.connection.set_onconnectionstatechange(None);
        self.connection.set_ontrack(None);
        for channel in self.channels.into_values() {
            channel.close();
        }
        self.connection.close();
    }
}

struct TimerHandle {
    browser_id: i32,
    _callback: Closure<dyn FnMut()>,
}

#[derive(Clone)]
struct BrowserHost {
    inner: Weak<RuntimeInner>,
}

impl BrowserHost {
    fn new(inner: &Rc<RuntimeInner>) -> Self {
        Self {
            inner: Rc::downgrade(inner),
        }
    }
}

impl Host for BrowserHost {
    fn publish_turn(&self, turn: &Turn) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        inner.publish_turn(turn);
    }

    fn execute_effect(&self, effect: Effect) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        inner.execute(effect);
    }

    fn host_failed(&self, failure: &Failure) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        inner.report_error(format!(
            "host failure ({:?}): {}",
            failure.class, failure.message
        ));
    }

    fn shutdown(&self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        inner.shutdown();
    }
}

struct RuntimeInner {
    actor: RefCell<Option<ActorHandle>>,
    local_slots: BTreeMap<String, MediaKind>,
    local_tracks: RefCell<BTreeMap<String, LocalTrackState>>,
    peers: RefCell<BTreeMap<u64, Peer>>,
    requests: RefCell<BTreeMap<u64, AbortController>>,
    timers: RefCell<BTreeMap<u64, TimerHandle>>,
    snapshot_listener: RefCell<Option<Function>>,
    event_listener: RefCell<Option<Function>>,
    error_listener: RefCell<Option<Function>>,
    last_error: RefCell<Option<String>>,
    closed: Cell<bool>,
}

#[wasm_bindgen]
pub struct BrowserRuntime {
    inner: Rc<RuntimeInner>,
}

#[wasm_bindgen]
impl BrowserRuntime {
    #[wasm_bindgen(constructor)]
    pub fn new(config: JsValue) -> Result<BrowserRuntime, JsValue> {
        let config: RuntimeConfig = serde_wasm_bindgen::from_value(config)
            .map_err(|error| js_error(format!("invalid browser runtime config: {error}")))?;
        let request_headers = config
            .request_headers
            .into_iter()
            .map(|(name, value)| HttpHeader { name, value })
            .collect();
        let topology = MediaTopology {
            local_video: config.topology.local_video,
            local_audio: config.topology.local_audio,
            remote_video: config.topology.remote_video,
            remote_audio: config.topology.remote_audio,
        };
        let local_slots = topology
            .local_video
            .iter()
            .cloned()
            .map(|slot| (slot, MediaKind::Video))
            .chain(
                topology
                    .local_audio
                    .iter()
                    .cloned()
                    .map(|slot| (slot, MediaKind::Audio)),
            )
            .collect();
        let core_config = AgentConfig {
            endpoint: config.endpoint,
            room_id: config.room_id,
            request_headers,
            topology,
            manual_subscriptions: true,
            retry: RetryPolicy::default(),
        };
        let inner = Rc::new(RuntimeInner {
            actor: RefCell::new(None),
            local_slots,
            local_tracks: RefCell::new(BTreeMap::new()),
            peers: RefCell::new(BTreeMap::new()),
            requests: RefCell::new(BTreeMap::new()),
            timers: RefCell::new(BTreeMap::new()),
            snapshot_listener: RefCell::new(None),
            event_listener: RefCell::new(None),
            error_listener: RefCell::new(None),
            last_error: RefCell::new(None),
            closed: Cell::new(false),
        });
        let actor = spawn_actor(core_config, BrowserHost::new(&inner))
            .map_err(|error| js_error(error.to_string()))?;
        *inner.actor.borrow_mut() = Some(actor);
        Ok(Self {
            inner,
        })
    }

    pub fn set_snapshot_listener(&self, listener: Option<Function>) {
        *self.inner.snapshot_listener.borrow_mut() = listener;
        let listener = self.inner.snapshot_listener.borrow().clone();
        if let Some(listener) = listener {
            let snapshot = snapshot_value(&self.inner.snapshot());
            call_listener(&listener, &snapshot, "snapshot");
        }
    }

    pub fn set_event_listener(&self, listener: Option<Function>) {
        *self.inner.event_listener.borrow_mut() = listener;
    }

    pub fn set_error_listener(&self, listener: Option<Function>) {
        *self.inner.error_listener.borrow_mut() = listener;
    }

    pub fn replace_desired(&self, desired: JsValue) -> Result<(), JsValue> {
        self.inner.ensure_open()?;
        let desired: DesiredConfig = serde_wasm_bindgen::from_value(desired)
            .map_err(|error| js_error(format!("invalid desired state: {error}")))?;
        self.inner
            .replace_desired(desired.into_core())
            .map_err(js_error)?;
        Ok(())
    }

    pub async fn replace_local_track(
        &self,
        slot: String,
        track: Option<MediaStreamTrack>,
        config: JsValue,
    ) -> Result<(), JsValue> {
        self.inner.ensure_open()?;
        let config: SenderConfig = serde_wasm_bindgen::from_value(config)
            .map_err(|error| js_error(format!("invalid sender configuration: {error}")))?;
        self.inner
            .replace_local_track(slot, track, config)
            .await
            .map_err(js_error)
    }

    pub async fn set_local_muted(&self, slot: String, muted: bool) -> Result<(), JsValue> {
        self.inner.ensure_open()?;
        self.inner
            .set_local_muted(slot, muted)
            .await
            .map_err(js_error)
    }

    pub fn remote_track(&self, mid: &str) -> Option<MediaStreamTrack> {
        let generation = self.inner.snapshot().generation?;
        self.inner
            .peers
            .borrow()
            .get(&generation.get())
            .and_then(|peer| peer.remote_tracks.get(mid))
            .cloned()
    }

    pub async fn statistics(&self) -> Result<JsValue, JsValue> {
        self.inner.ensure_open()?;
        self.inner.statistics().await.map_err(js_error)
    }

    pub fn connect(&self) -> Result<(), JsValue> {
        self.inner.ensure_open()?;
        self.inner
            .replace_connection_desired(true)
            .map_err(js_error)?;
        Ok(())
    }

    pub fn force_reconnect(&self) -> Result<(), JsValue> {
        self.inner.ensure_open()?;
        let generation = self.inner.snapshot().generation;
        let Some(generation) = generation else {
            return Err(js_error("cannot reconnect before a transport exists"));
        };
        self.inner.send_event(HostEvent::Rtc(RtcEvent::Disconnected { generation }));
        Ok(())
    }

    pub fn send_topic(&self, name: &str, mode: &str, payload: &[u8]) -> Result<(), JsValue> {
        self.inner.ensure_open()?;
        let mode = parse_topic_mode(mode)?;
        self.inner
            .send_topic(TopicSend {
            publisher: TopicPublisher {
                topic: name.to_owned(),
                mode,
            },
            payload: payload.to_vec(),
        })
        .map_err(js_error)?;
        Ok(())
    }

    pub fn close(&self) {
        if self.inner.closed.get() {
            return;
        }
        self.inner.request_close();
    }

    pub fn abort(&self) {
        self.inner.request_abort();
    }

    pub fn snapshot(&self) -> JsValue {
        snapshot_value(&self.inner.snapshot())
    }

    pub fn diagnostics(&self) -> JsValue {
        let value = Object::new();
        set(&value, "peers", self.inner.peers.borrow().len());
        set(&value, "requests", self.inner.requests.borrow().len());
        set(&value, "timers", self.inner.timers.borrow().len());
        set(&value, "closed", self.inner.closed.get());
        set(&value, "lastError", self.inner.last_error.borrow().clone());
        value.into()
    }
}

impl Drop for BrowserRuntime {
    fn drop(&mut self) {
        self.inner.request_abort();
    }
}

impl RuntimeInner {
    fn ensure_open(&self) -> Result<(), JsValue> {
        if self.closed.get() {
            Err(js_error("browser runtime is closed"))
        } else {
            Ok(())
        }
    }

    fn send_event(&self, event: HostEvent) {
        let actor = self.actor.borrow();
        let Some(actor) = actor.as_ref() else {
            return;
        };
        if let Err(error) = actor.send_host_event(event) {
            self.report_error(format!("runtime actor message failure: {error}"));
        }
    }

    fn send_topic(&self, send: TopicSend) -> Result<(), String> {
        let actor = self.actor.borrow();
        let Some(actor) = actor.as_ref() else {
            return Err("runtime actor not initialized".to_owned());
        };
        actor.send_topic(TopicCommand::SendTopic(send))
    }

    fn replace_desired(self: &Rc<Self>, desired: DesiredState) -> Result<(), String> {
        let actor = self.actor.borrow();
        let Some(actor) = actor.as_ref() else {
            return Err("runtime actor not initialized".to_owned());
        };
        actor.send_public(PublicCommand::ReplaceDesired(desired))
    }

    fn replace_connection_desired(self: &Rc<Self>, connected: bool) -> Result<(), String> {
        let actor = self.actor.borrow();
        let Some(actor) = actor.as_ref() else {
            return Err("runtime actor not initialized".to_owned());
        };
        actor.send_public(PublicCommand::SetConnected(connected))
    }

    fn request_close(&self) {
        let actor = self.actor.borrow();
        let Some(actor) = actor.as_ref() else {
            return;
        };
        if let Err(error) = actor.request_close() {
            self.report_error(format!("runtime actor message failure: {error}"));
        }
    }

    fn request_abort(&self) {
        let actor = self.actor.borrow();
        let Some(actor) = actor.as_ref() else {
            self.shutdown();
            return;
        };
        if let Err(error) = actor.abort() {
            self.report_error(format!("runtime actor message failure: {error}"));
            self.shutdown();
        }
    }

    fn snapshot(&self) -> agent_core::Snapshot {
        let actor = self.actor.borrow();
        actor
            .as_ref()
            .map_or_else(agent_core::Snapshot::default, |actor| actor.snapshot())
    }

    async fn replace_local_track(
        self: &Rc<Self>,
        slot: String,
        track: Option<MediaStreamTrack>,
        config: SenderConfig,
    ) -> Result<(), String> {
        let kind = self
            .local_slots
            .get(&slot)
            .copied()
            .ok_or_else(|| format!("unknown local publication slot: {slot}"))?;
        validate_sender_config(kind, &config)?;
        if let Some(track) = &track {
            let expected = media_kind_name(kind);
            if track.kind() != expected {
                return Err(format!(
                    "local slot {slot} requires a {expected} track, received {}",
                    track.kind()
                ));
            }
            let muted = self
                .local_tracks
                .borrow()
                .get(&slot)
                .is_some_and(|state| state.muted);
            track.set_enabled(!muted);
            set_property(
                track.as_ref(),
                "contentHint",
                &JsValue::from_str(&config.content_hint),
            )?;
            self.local_tracks.borrow_mut().insert(
                slot.clone(),
                LocalTrackState {
                    track: track.clone(),
                    config,
                    muted,
                },
            );
        } else {
            self.local_tracks.borrow_mut().remove(&slot);
        }
        self.sync_local_slot(&slot).await
    }

    async fn set_local_muted(self: &Rc<Self>, slot: String, muted: bool) -> Result<(), String> {
        let track = {
            let mut tracks = self.local_tracks.borrow_mut();
            let state = tracks
                .get_mut(&slot)
                .ok_or_else(|| format!("local publication slot has no track: {slot}"))?;
            if state.muted == muted {
                return Ok(());
            }
            state.muted = muted;
            state.track.clone()
        };
        track.set_enabled(!muted);
        self.sync_local_slot(&slot).await
    }

    async fn sync_local_slot(&self, slot: &str) -> Result<(), String> {
        let state = self.local_tracks.borrow().get(slot).cloned();
        let senders: Vec<RtcRtpSender> = self
            .peers
            .borrow()
            .values()
            .flat_map(|peer| &peer.transceivers)
            .filter_map(|(media_slot, transceiver)| match media_slot {
                MediaSlot::LocalVideo(name) | MediaSlot::LocalAudio(name) if name == slot => {
                    Some(transceiver.sender())
                }
                _ => None,
            })
            .collect();
        for sender in senders {
            apply_sender_state(&sender, state.as_ref()).await?;
        }
        Ok(())
    }

    async fn prepare_peer(self: &Rc<Self>, generation: Generation) {
        let slots: Vec<String> = self.local_slots.keys().cloned().collect();
        for slot in slots {
            if let Err(error) = self.sync_local_slot(&slot).await {
                self.rtc_failed(generation, error);
                return;
            }
        }
        self.create_offer(generation);
    }

    async fn statistics(&self) -> Result<JsValue, String> {
        let generation = self
            .snapshot()
            .generation
            .ok_or_else(|| "statistics are unavailable before a transport exists".to_owned())?;
        let (connection, senders) = {
            let peers = self.peers.borrow();
            let peer = peers
                .get(&generation.get())
                .ok_or_else(|| "active transport is unavailable".to_owned())?;
            let senders = peer
                .transceivers
                .iter()
                .filter_map(|(slot, transceiver)| match slot {
                    MediaSlot::LocalVideo(name) => {
                        Some((name.clone(), "video", transceiver.sender()))
                    }
                    MediaSlot::LocalAudio(name) => {
                        Some((name.clone(), "audio", transceiver.sender()))
                    }
                    MediaSlot::RemoteVideo(_) | MediaSlot::RemoteAudio(_) => None,
                })
                .collect::<Vec<_>>();
            (peer.connection.clone(), senders)
        };
        let report = JsFuture::from(connection.get_stats())
            .await
            .map_err(js_message)?;
        let value = Object::new();
        set(
            &value,
            "connection",
            connection_name_from_browser(&connection),
        );
        set(&value, "report", report);
        let sender_values = Array::new();
        for (slot, kind, sender) in senders {
            let sender_value = Object::new();
            set(&sender_value, "slot", slot);
            set(&sender_value, "kind", kind);
            set(
                &sender_value,
                "trackId",
                sender.track().map(|track| track.id()),
            );
            set(&sender_value, "parameters", sender.get_parameters());
            sender_values.push(&sender_value);
        }
        set(&value, "senders", sender_values);
        Ok(value.into())
    }

    fn publish_turn(&self, turn: &Turn) {
        if let Some(error) = &turn.error {
            self.report_error(format!("core rejected input: {error}"));
        }
        let snapshot_listener = self.snapshot_listener.borrow().clone();
        if let Some(snapshot) = &turn.snapshot
            && let Some(listener) = snapshot_listener
        {
            call_listener(&listener, &snapshot_value(snapshot), "snapshot");
        }
        let event_listener = self.event_listener.borrow().clone();
        if let Some(listener) = event_listener {
            for notification in &turn.notifications {
                if self.closed.get() {
                    break;
                }
                call_listener(&listener, &notification_value(notification), "event");
            }
        }
    }

    fn publish_current_snapshot(&self) {
        let listener = self.snapshot_listener.borrow().clone();
        if let Some(listener) = listener {
            let snapshot = snapshot_value(&self.snapshot());
            call_listener(&listener, &snapshot, "snapshot");
        }
    }

    fn execute(self: &Rc<Self>, effect: Effect) {
        if self.closed.get() {
            return;
        }
        match effect {
            Effect::Rtc(effect) => self.execute_rtc(effect),
            Effect::Http(effect) => self.execute_http(effect),
            Effect::Timer(effect) => self.execute_timer(effect),
            Effect::DataChannel(effect) => self.execute_data_channel(effect),
        }
    }

    fn execute_rtc(self: &Rc<Self>, effect: RtcEffect) {
        match effect {
            RtcEffect::CreateOffer {
                generation,
                topology,
                data_channels,
            } => match self.build_peer(generation, &topology, &data_channels) {
                Ok(peer) => {
                    let previous = self.peers.borrow_mut().insert(generation.get(), peer);
                    debug_assert!(previous.is_none(), "generation must own one peer");
                    let inner = Rc::clone(self);
                    spawn_local(async move {
                        inner.prepare_peer(generation).await;
                    });
                }
                Err(error) => self.rtc_failed(generation, error),
            },
            RtcEffect::ApplyAnswer { generation, answer } => {
                self.apply_answer(generation, answer);
            }
            RtcEffect::Close { generation } => {
                if let Some(peer) = self.peers.borrow_mut().remove(&generation.get()) {
                    peer.close();
                }
                self.send_event(HostEvent::Rtc(RtcEvent::Closed { generation }));
            }
        }
    }

    fn build_peer(
        self: &Rc<Self>,
        generation: Generation,
        topology: &MediaTopology,
        specs: &[DataChannelSpec],
    ) -> Result<Peer, String> {
        let config = RtcConfiguration::new();
        config.set_bundle_policy(RtcBundlePolicy::MaxBundle);
        config.set_ice_servers(&Array::new());
        let connection = RtcPeerConnection::new_with_configuration(&config).map_err(js_message)?;

        let weak = Rc::downgrade(self);
        let state_connection = connection.clone();
        let state = Closure::wrap(Box::new(move |_event: Event| {
            let Some(inner) = weak.upgrade() else {
                return;
            };
            match state_connection.connection_state() {
                RtcPeerConnectionState::Connected => {
                    inner.send_event(HostEvent::Rtc(RtcEvent::Connected { generation }));
                }
                RtcPeerConnectionState::Disconnected | RtcPeerConnectionState::Failed => {
                    inner.send_event(HostEvent::Rtc(RtcEvent::Disconnected { generation }));
                }
                RtcPeerConnectionState::New
                | RtcPeerConnectionState::Connecting
                | RtcPeerConnectionState::Closed
                | _ => {}
            }
        }) as Box<dyn FnMut(Event)>);
        connection.set_onconnectionstatechange(Some(state.as_ref().unchecked_ref()));

        let track_weak = Rc::downgrade(self);
        let track = Closure::wrap(Box::new(move |event: RtcTrackEvent| {
            let Some(inner) = track_weak.upgrade() else {
                return;
            };
            let Some(mid) = event.transceiver().mid() else {
                inner.report_error(format!(
                    "generation={} received a remote track without a MID",
                    generation.get()
                ));
                return;
            };
            let inserted = inner
                .peers
                .borrow_mut()
                .get_mut(&generation.get())
                .map(|peer| peer.remote_tracks.insert(mid, event.track()))
                .is_some();
            if inserted {
                inner.publish_current_snapshot();
            }
        }) as Box<dyn FnMut(RtcTrackEvent)>);
        connection.set_ontrack(Some(track.as_ref().unchecked_ref()));

        let mut channels = BTreeMap::new();
        for (index, spec) in specs.iter().enumerate() {
            let numeric = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
            let Some(channel_id) = ChannelId::new(numeric) else {
                return Err("data channel ID space exhausted".into());
            };
            let channel = self.create_data_channel(generation, channel_id, &connection, spec);
            let previous = channels.insert(channel_id.get(), channel?);
            debug_assert!(previous.is_none(), "channel IDs must be unique");
        }

        let mut transceivers = Vec::new();
        for name in &topology.local_video {
            let slot = MediaSlot::LocalVideo(name.clone());
            let transceiver = add_transceiver(&connection, "video", true, true)?;
            transceivers.push((slot, transceiver));
        }
        for name in &topology.local_audio {
            let slot = MediaSlot::LocalAudio(name.clone());
            let transceiver = add_transceiver(&connection, "audio", true, false)?;
            transceivers.push((slot, transceiver));
        }
        for index in 0..topology.remote_video {
            let transceiver = add_transceiver(&connection, "video", false, false)?;
            transceivers.push((MediaSlot::RemoteVideo(index), transceiver));
        }
        for index in 0..topology.remote_audio {
            let transceiver = add_transceiver(&connection, "audio", false, false)?;
            transceivers.push((MediaSlot::RemoteAudio(index), transceiver));
        }

        Ok(Peer {
            connection,
            transceivers,
            remote_tracks: BTreeMap::new(),
            channels,
            _state: state,
            _track: track,
        })
    }

    fn create_data_channel(
        self: &Rc<Self>,
        generation: Generation,
        channel_id: ChannelId,
        connection: &RtcPeerConnection,
        spec: &DataChannelSpec,
    ) -> Result<DataChannel, String> {
        let init = RtcDataChannelInit::new();
        init.set_ordered(spec.ordered);
        match spec.reliability {
            DataChannelReliability::Reliable => {}
            DataChannelReliability::MaxRetransmits(value) => init.set_max_retransmits(value),
            DataChannelReliability::MaxPacketLifetime(value) => {
                init.set_max_packet_life_time(value);
            }
        }
        let channel = connection.create_data_channel_with_data_channel_dict(&spec.label, &init);
        channel.set_binary_type(RtcDataChannelType::Arraybuffer);

        let open_weak = Rc::downgrade(self);
        let open = Closure::wrap(Box::new(move |_event: Event| {
            if let Some(inner) = open_weak.upgrade() {
                inner.send_event(HostEvent::DataChannel(DataChannelEvent::Opened {
                    generation,
                    channel: channel_id,
                }));
            }
        }) as Box<dyn FnMut(Event)>);
        channel.set_onopen(Some(open.as_ref().unchecked_ref()));

        let close_weak = Rc::downgrade(self);
        let close = Closure::wrap(Box::new(move |_event: Event| {
            if let Some(inner) = close_weak.upgrade() {
                inner.send_event(HostEvent::DataChannel(DataChannelEvent::Closed {
                    generation,
                    channel: channel_id,
                }));
            }
        }) as Box<dyn FnMut(Event)>);
        channel.set_onclose(Some(close.as_ref().unchecked_ref()));

        let message_weak = Rc::downgrade(self);
        let message = Closure::wrap(Box::new(move |event: MessageEvent| {
            let Some(inner) = message_weak.upgrade() else {
                return;
            };
            let data = event.data();
            if data.is_instance_of::<js_sys::ArrayBuffer>() || data.is_instance_of::<Uint8Array>() {
                let payload = Uint8Array::new(&data).to_vec();
                inner.send_event(HostEvent::DataChannel(DataChannelEvent::Message {
                    generation,
                    channel: channel_id,
                    payload,
                }));
            } else {
                inner.report_error(format!(
                    "generation={} channel={} received non-binary data",
                    generation.get(),
                    channel_id.get()
                ));
            }
        }) as Box<dyn FnMut(MessageEvent)>);
        channel.set_onmessage(Some(message.as_ref().unchecked_ref()));

        Ok(DataChannel {
            channel,
            _open: open,
            _close: close,
            _message: message,
        })
    }

    fn create_offer(self: &Rc<Self>, generation: Generation) {
        let Some(connection) = self
            .peers
            .borrow()
            .get(&generation.get())
            .map(|peer| peer.connection.clone())
        else {
            self.rtc_failed(generation, "offer references an unknown generation".into());
            return;
        };
        let weak = Rc::downgrade(self);
        spawn_local(async move {
            let result = async {
                let offer_value = JsFuture::from(connection.create_offer()).await?;
                let offer: RtcSessionDescriptionInit = offer_value.unchecked_into();
                JsFuture::from(connection.set_local_description(&offer)).await?;
                let description = connection
                    .local_description()
                    .ok_or_else(|| js_error("browser omitted local description"))?;
                Ok::<String, JsValue>(description.sdp())
            }
            .await;
            if let Some(inner) = weak.upgrade() {
                if !inner.peers.borrow().contains_key(&generation.get()) {
                    return;
                }
                match result {
                    Ok(offer) => match inner.offer_resources(generation) {
                        Ok(resources) => {
                            inner.send_event(HostEvent::Rtc(RtcEvent::OfferCreated {
                                generation,
                                offer,
                                resources,
                            }));
                        }
                        Err(error) => inner.rtc_failed(generation, error),
                    },
                    Err(error) => inner.rtc_failed(generation, js_message(error)),
                }
            }
        });
    }

    fn offer_resources(&self, generation: Generation) -> Result<OfferResources, String> {
        let peers = self.peers.borrow();
        let peer = peers
            .get(&generation.get())
            .ok_or_else(|| "offer completed for an obsolete generation".to_owned())?;
        let mut slots = Vec::with_capacity(peer.transceivers.len());
        for (slot, transceiver) in &peer.transceivers {
            let mid = transceiver
                .mid()
                .ok_or_else(|| "browser did not assign a MID after local description".to_owned())?;
            slots.push(SlotBinding {
                slot: slot.clone(),
                mid,
            });
        }
        let signaling_channel = peer
            .channels
            .iter()
            .find_map(|(id, channel)| {
                if channel.channel.label() == SIGNALING_LABEL {
                    ChannelId::new(*id)
                } else {
                    None
                }
            })
            .ok_or_else(|| "offer has no signaling data channel".to_owned())?;
        let mut data_channels = Vec::new();
        for (id, channel) in &peer.channels {
            if *id == signaling_channel.get() {
                continue;
            }
            let Some(channel_id) = ChannelId::new(*id) else {
                return Err("data channel ID is invalid".into());
            };
            data_channels.push(DataChannelBinding {
                label: channel.channel.label(),
                channel: channel_id,
            });
        }
        Ok(OfferResources {
            slots,
            signaling_channel,
            data_channels,
        })
    }

    fn apply_answer(self: &Rc<Self>, generation: Generation, answer: String) {
        let Some(connection) = self
            .peers
            .borrow()
            .get(&generation.get())
            .map(|peer| peer.connection.clone())
        else {
            self.rtc_failed(generation, "answer references an unknown generation".into());
            return;
        };
        let weak = Rc::downgrade(self);
        spawn_local(async move {
            let description = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
            description.set_sdp(&answer);
            let result = JsFuture::from(connection.set_remote_description(&description)).await;
            if let Some(inner) = weak.upgrade() {
                if !inner.peers.borrow().contains_key(&generation.get()) {
                    return;
                }
                match result {
                    Ok(_) => inner.send_event(HostEvent::Rtc(RtcEvent::AnswerApplied { generation })),
                    Err(error) => inner.rtc_failed(generation, js_message(error)),
                }
            }
        });
    }

    fn rtc_failed(self: &Rc<Self>, generation: Generation, message: String) {
        self.report_error(format!(
            "generation={} RTC operation failed: {message}",
            generation.get()
        ));
        self.send_event(HostEvent::Rtc(RtcEvent::Failed { generation, message }));
    }

    fn execute_http(self: &Rc<Self>, effect: HttpEffect) {
        match effect {
            HttpEffect::Request {
                operation,
                generation,
                request,
            } => {
                let controller = match AbortController::new() {
                    Ok(controller) => controller,
                    Err(error) => {
                        self.http_failed(operation, js_message(error));
                        return;
                    }
                };
                let browser_request = match browser_request(&request, &controller) {
                    Ok(request) => request,
                    Err(error) => {
                        self.http_failed(operation, error);
                        return;
                    }
                };
                let previous = self
                    .requests
                    .borrow_mut()
                    .insert(operation.get(), controller);
                debug_assert!(previous.is_none(), "operation must own one fetch");
                log::debug!(
                    "starting browser HTTP request operation={} generation={:?} method={:?}",
                    operation.get(),
                    generation.map(Generation::get),
                    request.method,
                );
                let weak = Rc::downgrade(self);
                spawn_local(async move {
                    let result = fetch(browser_request).await;
                    let Some(inner) = weak.upgrade() else {
                        return;
                    };
                    if inner
                        .requests
                        .borrow_mut()
                        .remove(&operation.get())
                        .is_none()
                    {
                        return;
                    }
                    match result {
                        Ok(response) => {
                            inner.send_event(HostEvent::Http(HttpEvent::Response {
                                operation,
                                response,
                            }));
                        }
                        Err(error) => inner.http_failed(operation, error),
                    }
                });
            }
            HttpEffect::Cancel { operation } => {
                if let Some(controller) = self.requests.borrow_mut().remove(&operation.get()) {
                    controller.abort();
                }
            }
        }
    }

    fn http_failed(self: &Rc<Self>, operation: OperationId, message: String) {
        self.report_error(format!(
            "browser HTTP request failed operation={}: {message}",
            operation.get()
        ));
        self.send_event(HostEvent::Http(HttpEvent::Failed { operation, message }));
    }

    fn execute_timer(self: &Rc<Self>, effect: TimerEffect) {
        let Some(window) = web_sys::window() else {
            self.report_error("browser runtime requires Window".into());
            return;
        };
        match effect {
            TimerEffect::Schedule { timer, after } => {
                let weak = Rc::downgrade(self);
                let callback = Closure::wrap(Box::new(move || {
                    let Some(inner) = weak.upgrade() else {
                        return;
                    };
                    let handle = inner.timers.borrow_mut().remove(&timer.get());
                    if handle.is_some() {
                        inner.send_event(HostEvent::Timer(TimerEvent::Fired { timer }));
                    }
                }) as Box<dyn FnMut()>);
                let delay = i32::try_from(after.as_millis()).unwrap_or(i32::MAX);
                match window.set_timeout_with_callback_and_timeout_and_arguments_0(
                    callback.as_ref().unchecked_ref(),
                    delay,
                ) {
                    Ok(browser_id) => {
                        let previous = self.timers.borrow_mut().insert(
                            timer.get(),
                            TimerHandle {
                                browser_id,
                                _callback: callback,
                            },
                        );
                        debug_assert!(previous.is_none(), "timer ID must be unique");
                    }
                    Err(error) => {
                        self.report_error(format!(
                            "browser timer schedule failed timer={}: {}",
                            timer.get(),
                            js_message(error)
                        ));
                    }
                }
            }
            TimerEffect::Cancel { timer } => {
                if let Some(handle) = self.timers.borrow_mut().remove(&timer.get()) {
                    window.clear_timeout_with_handle(handle.browser_id);
                }
            }
        }
    }

    fn execute_data_channel(self: &Rc<Self>, effect: DataChannelEffect) {
        let DataChannelEffect::Send {
            operation,
            generation,
            channel,
            binary,
            payload,
        } = effect;
        let data_channel = self.peers.borrow().get(&generation.get()).and_then(|peer| {
            peer.channels
                .get(&channel.get())
                .map(|data_channel| data_channel.channel.clone())
        });
        let result = data_channel
            .ok_or_else(|| "data channel generation is no longer active".to_owned())
            .and_then(|data_channel| {
                if !binary {
                    return Err("core requested a non-binary data-channel send".into());
                }
                data_channel
                    .send_with_u8_array(&payload)
                    .map_err(js_message)
            });
        let event = match result {
            Ok(()) => DataChannelEvent::Sent {
                operation,
                generation,
                channel,
            },
            Err(message) => DataChannelEvent::SendFailed {
                operation,
                generation,
                channel,
                message,
            },
        };
        self.send_event(HostEvent::DataChannel(event));
    }

    fn report_error(&self, message: String) {
        log::warn!("{message}");
        *self.last_error.borrow_mut() = Some(message.clone());
        let listener = self.error_listener.borrow().clone();
        if let Some(listener) = listener {
            call_listener(&listener, &JsValue::from_str(&message), "error");
        }
    }

    fn shutdown(&self) {
        if self.closed.replace(true) {
            return;
        }
        self.local_tracks.borrow_mut().clear();
        for (_, peer) in std::mem::take(&mut *self.peers.borrow_mut()) {
            peer.close();
        }
        for (_, request) in std::mem::take(&mut *self.requests.borrow_mut()) {
            request.abort();
        }
        if let Some(window) = web_sys::window() {
            for (_, timer) in std::mem::take(&mut *self.timers.borrow_mut()) {
                window.clear_timeout_with_handle(timer.browser_id);
            }
        } else {
            self.timers.borrow_mut().clear();
        }
        self.snapshot_listener.borrow_mut().take();
        self.event_listener.borrow_mut().take();
        self.error_listener.borrow_mut().take();
        log::info!("browser agent runtime aborted");
    }
}

fn topic_registrations(topics: &[TopicRegistrationConfig]) -> TopicRegistrations {
    let mut registrations = TopicRegistrations::default();
    for topic in topics {
        let mode = topic.mode.into();
        if topic.publish {
            registrations.publishers.push(TopicPublisher {
                topic: topic.name.clone(),
                mode,
            });
        }
        if topic.subscribe {
            registrations.subscribers.push(TopicSubscriber {
                topic: topic.name.clone(),
                mode,
                publisher_id: topic.publisher_id.clone(),
            });
        }
    }
    registrations
}

fn parse_topic_mode(mode: &str) -> Result<TopicMode, JsValue> {
    match mode {
        "latest" => Ok(TopicMode::Latest),
        "ordered" => Ok(TopicMode::Ordered),
        _ => Err(js_error("topic mode must be latest or ordered")),
    }
}

fn add_transceiver(
    connection: &RtcPeerConnection,
    kind: &str,
    send: bool,
    simulcast: bool,
) -> Result<RtcRtpTransceiver, String> {
    let init = RtcRtpTransceiverInit::new();
    init.set_direction(if send {
        RtcRtpTransceiverDirection::Sendonly
    } else {
        RtcRtpTransceiverDirection::Recvonly
    });
    if simulcast {
        let encodings = Array::new();
        for rid in ["q", "h", "f"] {
            let encoding = Object::new();
            set(&encoding, "rid", rid);
            set(&encoding, "active", false);
            set(&encoding, "scalabilityMode", "L1T3");
            encodings.push(&encoding);
        }
        init.set_send_encodings(&encodings);
    }
    let method: Function = Reflect::get(connection.as_ref(), &JsValue::from_str("addTransceiver"))
        .map_err(js_message)?
        .dyn_into()
        .map_err(js_message)?;
    method
        .call2(connection.as_ref(), &JsValue::from_str(kind), init.as_ref())
        .map_err(js_message)?
        .dyn_into()
        .map_err(js_message)
}

fn validate_sender_config(kind: MediaKind, config: &SenderConfig) -> Result<(), String> {
    let valid_hint = match kind {
        MediaKind::Video => matches!(config.content_hint.as_str(), "motion" | "detail" | "text"),
        MediaKind::Audio => matches!(config.content_hint.as_str(), "speech" | "music"),
    };
    if !valid_hint {
        return Err(format!(
            "invalid {} content hint: {}",
            media_kind_name(kind),
            config.content_hint
        ));
    }
    let expected = match kind {
        MediaKind::Video => 3,
        MediaKind::Audio => 1,
    };
    if config.encodings.len() != expected {
        return Err(format!(
            "{} sender configuration requires {expected} encoding entries",
            media_kind_name(kind)
        ));
    }
    for encoding in &config.encodings {
        for value in [
            encoding.scale_resolution_down_by,
            encoding.max_bitrate,
            encoding.max_framerate,
        ]
        .into_iter()
        .flatten()
        {
            if !value.is_finite() || value <= 0.0 {
                return Err("sender encoding values must be finite and positive".into());
            }
        }
        if encoding
            .scalability_mode
            .as_deref()
            .is_some_and(|mode| !matches!(mode, "L1T1" | "L1T2" | "L1T3"))
        {
            return Err("sender scalability mode must be L1T1, L1T2, or L1T3".into());
        }
        if encoding
            .dtx
            .as_deref()
            .is_some_and(|dtx| !matches!(dtx, "enabled" | "disabled"))
        {
            return Err("audio DTX must be enabled or disabled".into());
        }
    }
    Ok(())
}

async fn apply_sender_state(
    sender: &RtcRtpSender,
    state: Option<&LocalTrackState>,
) -> Result<(), String> {
    let track = state.map(|state| &state.track);
    JsFuture::from(sender.replace_track(track))
        .await
        .map_err(js_message)?;
    let Some(state) = state else {
        return Ok(());
    };
    set_property(
        state.track.as_ref(),
        "contentHint",
        &JsValue::from_str(&state.config.content_hint),
    )?;
    apply_sender_parameters(sender, state, false).await?;
    if state
        .config
        .encodings
        .iter()
        .any(|encoding| encoding.scalability_mode.is_some())
        && let Err(error) = apply_sender_parameters(sender, state, true).await
    {
        log::warn!("browser rejected sender scalability mode: {error}");
    }
    Ok(())
}

async fn apply_sender_parameters(
    sender: &RtcRtpSender,
    state: &LocalTrackState,
    scalability_only: bool,
) -> Result<(), String> {
    let parameters = sender.get_parameters();
    let encodings: Array = Reflect::get(parameters.as_ref(), &JsValue::from_str("encodings"))
        .map_err(js_message)?
        .dyn_into()
        .map_err(js_message)?;
    for (index, value) in encodings.iter().enumerate() {
        let Some(config) = encoding_for(&state.config.encodings, &value, index) else {
            continue;
        };
        if scalability_only {
            if let Some(mode) = &config.scalability_mode {
                set_property(&value, "scalabilityMode", &JsValue::from_str(mode))?;
            }
            continue;
        }
        set_property(
            &value,
            "active",
            &JsValue::from_bool(config.active && !state.muted),
        )?;
        set_optional_number(
            &value,
            "scaleResolutionDownBy",
            config.scale_resolution_down_by,
        )?;
        set_optional_number(&value, "maxBitrate", config.max_bitrate)?;
        set_optional_number(&value, "maxFramerate", config.max_framerate)?;
        if let Some(dtx) = &config.dtx {
            set_property(&value, "dtx", &JsValue::from_str(dtx))?;
        }
    }
    if !scalability_only && let Some(preference) = &state.config.degradation_preference {
        set_property(
            parameters.as_ref(),
            "degradationPreference",
            &JsValue::from_str(preference),
        )?;
    }
    JsFuture::from(sender.set_parameters_with_parameters(&parameters))
        .await
        .map_err(js_message)?;
    Ok(())
}

fn encoding_for<'a>(
    configs: &'a [EncodingConfig],
    value: &JsValue,
    index: usize,
) -> Option<&'a EncodingConfig> {
    let rid = Reflect::get(value, &JsValue::from_str("rid"))
        .ok()
        .and_then(|value| value.as_string());
    rid.as_deref()
        .and_then(|rid| {
            configs
                .iter()
                .find(|config| config.rid.as_deref() == Some(rid))
        })
        .or_else(|| configs.get(index))
}

fn set_optional_number(object: &JsValue, name: &str, value: Option<f64>) -> Result<(), String> {
    if let Some(value) = value {
        set_property(object, name, &JsValue::from_f64(value))?;
    }
    Ok(())
}

fn set_property(object: &JsValue, name: &str, value: &JsValue) -> Result<(), String> {
    Reflect::set(object, &JsValue::from_str(name), value)
        .map_err(js_message)
        .and_then(|set| {
            if set {
                Ok(())
            } else {
                Err(format!("browser rejected sender property: {name}"))
            }
        })
}

fn media_kind_name(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Video => "video",
        MediaKind::Audio => "audio",
    }
}

fn connection_name_from_browser(connection: &RtcPeerConnection) -> &'static str {
    match connection.connection_state() {
        RtcPeerConnectionState::New => "new",
        RtcPeerConnectionState::Connecting => "connecting",
        RtcPeerConnectionState::Connected => "connected",
        RtcPeerConnectionState::Disconnected => "disconnected",
        RtcPeerConnectionState::Failed => "failed",
        RtcPeerConnectionState::Closed => "closed",
        _ => "unknown",
    }
}

fn browser_request(
    request: &agent_core::HttpRequest,
    controller: &AbortController,
) -> Result<Request, String> {
    let init = RequestInit::new();
    let method = match request.method {
        HttpMethod::Post => "POST",
        HttpMethod::Patch => "PATCH",
        HttpMethod::Delete => "DELETE",
    };
    init.set_method(method);
    init.set_signal(Some(&controller.signal()));
    if !request.body.is_empty() {
        let body = Uint8Array::from(request.body.as_slice());
        init.set_body(&body);
    }
    let headers = Headers::new().map_err(js_message)?;
    for header in &request.headers {
        headers
            .append(&header.name, &header.value)
            .map_err(js_message)?;
    }
    init.set_headers(&headers);
    Request::new_with_str_and_init(&request.uri, &init).map_err(js_message)
}

async fn fetch(request: Request) -> Result<HttpResponse, String> {
    let window = web_sys::window().ok_or_else(|| "browser runtime requires Window".to_owned())?;
    let response = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(js_message)?;
    let response: Response = response
        .dyn_into()
        .map_err(|value| format!("fetch returned a non-Response value: {}", js_message(value)))?;
    let mut headers = Vec::new();
    for name in ["location", "etag", "pb-participant-id", "content-type"] {
        if let Some(value) = response.headers().get(name).map_err(js_message)? {
            headers.push(HttpHeader {
                name: name.to_owned(),
                value,
            });
        }
    }
    let body = JsFuture::from(response.array_buffer().map_err(js_message)?)
        .await
        .map_err(js_message)?;
    Ok(HttpResponse {
        status: response.status(),
        headers,
        body: Uint8Array::new(&body).to_vec(),
    })
}

fn snapshot_value(snapshot: &agent_core::Snapshot) -> JsValue {
    let value = Object::new();
    set(&value, "version", snapshot.version);
    set(&value, "desiredRevision", snapshot.desired_revision);
    set(&value, "connection", connection_name(&snapshot.connection));
    set(
        &value,
        "generation",
        snapshot.generation.map(Generation::get),
    );
    set(&value, "participantId", snapshot.participant_id.clone());
    let participants = Array::new();
    for participant in snapshot.participants.values() {
        let item = Object::new();
        set(&item, "id", participant.id.clone());
        participants.push(&item);
    }
    set(&value, "participants", participants);
    let publications = Array::new();
    for publication in snapshot.publications.values() {
        let item = Object::new();
        set(&item, "id", publication.id.clone());
        set(&item, "participantId", publication.participant_id.clone());
        set(&item, "kind", media_kind_name(publication.kind));
        publications.push(&item);
    }
    set(&value, "publications", publications);
    let video = Array::new();
    for binding in snapshot.video.values() {
        let item = Object::new();
        set(&item, "trackId", binding.track_id.clone());
        set(&item, "mid", binding.mid.clone());
        set(&item, "paused", binding.paused);
        video.push(&item);
    }
    set(&value, "video", video);
    let audio = Array::new();
    for binding in &snapshot.audio {
        let item = Object::new();
        set(&item, "trackId", binding.track_id.clone());
        set(&item, "mid", binding.mid.clone());
        set(&item, "levelDbov", binding.level_dbov);
        audio.push(&item);
    }
    set(&value, "audio", audio);
    set(&value, "topics", topic_snapshot_value(&snapshot.topics));
    set(
        &value,
        "failure",
        snapshot.terminal_failure.as_ref().map(failure_value),
    );
    value.into()
}

fn topic_snapshot_value(snapshot: &agent_core::TopicSnapshot) -> JsValue {
    let value = Object::new();
    let publishers = Array::new();
    for status in &snapshot.publishers {
        let item = Object::new();
        set(&item, "name", status.registration.topic.clone());
        set(&item, "mode", topic_mode_name(status.registration.mode));
        set(&item, "connected", status.channel.is_some());
        set(&item, "streamId", status.stream_id);
        set(&item, "nextSequence", status.next_sequence);
        set(&item, "queued", status.queued_messages);
        set(&item, "sendPending", status.send_pending);
        publishers.push(&item);
    }
    set(&value, "publishers", publishers);
    let subscribers = Array::new();
    for status in &snapshot.subscribers {
        let item = Object::new();
        set(&item, "name", status.registration.topic.clone());
        set(&item, "mode", topic_mode_name(status.registration.mode));
        set(
            &item,
            "publisherId",
            status.registration.publisher_id.clone(),
        );
        set(&item, "connected", status.channel.is_some());
        set(&item, "publishers", status.publishers);
        set(&item, "buffered", status.buffered_messages);
        subscribers.push(&item);
    }
    set(&value, "subscribers", subscribers);
    set(&value, "acceptedSends", snapshot.accepted_sends);
    set(&value, "droppedSends", snapshot.dropped_sends);
    set(&value, "deliveredMessages", snapshot.delivered_messages);
    set(&value, "resynchronizations", snapshot.resynchronizations);
    set(&value, "channelFailures", snapshot.channel_failures);
    value.into()
}

fn failure_value(failure: &agent_core::Failure) -> JsValue {
    let value = Object::new();
    set(&value, "class", failure_class_name(failure.class));
    set(&value, "message", failure.message.clone());
    value.into()
}

fn notification_value(notification: &Notification) -> JsValue {
    let value = Object::new();
    match notification {
        Notification::Topic(TopicNotification::Message(message)) => {
            set(&value, "type", "topic-message");
            match message {
                TopicMessage::Latest {
                    topic,
                    publisher_id,
                    payload,
                } => {
                    set(&value, "mode", "latest");
                    set(&value, "topic", topic);
                    set(&value, "publisherId", publisher_id.clone());
                    set(&value, "payload", Uint8Array::from(payload.as_slice()));
                }
                TopicMessage::Ordered {
                    topic,
                    publisher_id,
                    stream_id,
                    sequence,
                    payload,
                } => {
                    set(&value, "mode", "ordered");
                    set(&value, "topic", topic);
                    set(&value, "publisherId", publisher_id);
                    set(&value, "streamId", *stream_id);
                    set(&value, "sequence", *sequence);
                    set(&value, "payload", Uint8Array::from(payload.as_slice()));
                }
            }
        }
        Notification::Topic(TopicNotification::Resynchronized {
            subscriber,
            publisher_id,
            stream_id,
            next_sequence,
        }) => {
            set(&value, "type", "topic-resynchronized");
            set(&value, "topic", &subscriber.topic);
            set(&value, "publisherId", publisher_id);
            set(&value, "streamId", *stream_id);
            set(&value, "nextSequence", *next_sequence);
        }
        Notification::Topic(TopicNotification::SendAdmitted {
            publisher,
            operation,
            stream_id,
            sequence,
        }) => {
            set(&value, "type", "topic-send-admitted");
            set(&value, "topic", publisher.topic.clone());
            set(&value, "mode", topic_mode_name(publisher.mode));
            set(&value, "operation", operation.get());
            set(&value, "streamId", *stream_id);
            set(&value, "sequence", *sequence);
        }
        Notification::Topic(TopicNotification::SendDropped { publisher, reason }) => {
            set(&value, "type", "topic-send-dropped");
            set(&value, "topic", publisher.topic.clone());
            set(&value, "mode", topic_mode_name(publisher.mode));
            set(&value, "reason", topic_drop_reason_name(*reason));
        }
        Notification::Topic(TopicNotification::ChannelFailed { channel, message }) => {
            set(&value, "type", "topic-channel-failed");
            match channel {
                TopicChannel::Publisher(publisher) => {
                    set(&value, "direction", "publish");
                    set(&value, "topic", publisher.topic.clone());
                    set(&value, "mode", topic_mode_name(publisher.mode));
                }
                TopicChannel::Subscriber(subscriber) => {
                    set(&value, "direction", "subscribe");
                    set(&value, "topic", subscriber.topic.clone());
                    set(&value, "mode", topic_mode_name(subscriber.mode));
                }
            }
            set(&value, "message", message.clone());
        }
        Notification::Failure(failure) => {
            set(&value, "type", "failure");
            set(&value, "message", &failure.message);
            set(&value, "class", failure_class_name(failure.class));
        }
        _ => {
            set(&value, "type", "state-change");
        }
    }
    value.into()
}

fn connection_name(state: &ConnectionState) -> String {
    match state {
        ConnectionState::Disconnected => "disconnected".into(),
        ConnectionState::CreatingOffer => "creating-offer".into(),
        ConnectionState::Joining => "joining".into(),
        ConnectionState::ApplyingAnswer => "applying-answer".into(),
        ConnectionState::WaitingForTransport => "waiting-for-transport".into(),
        ConnectionState::WaitingForSignaling => "waiting-for-signaling".into(),
        ConnectionState::Connected => "connected".into(),
        ConnectionState::Reconnecting => "reconnecting".into(),
        ConnectionState::RetryWaiting { attempt, .. } => format!("retry-waiting:{attempt}"),
        ConnectionState::Closing => "closing".into(),
        ConnectionState::TerminalFailure => "terminal-failure".into(),
    }
}

fn topic_mode_name(mode: TopicMode) -> &'static str {
    match mode {
        TopicMode::Latest => "latest",
        TopicMode::Ordered => "ordered",
    }
}

fn failure_class_name(class: FailureClass) -> &'static str {
    match class {
        FailureClass::InvalidConfiguration => "invalid-configuration",
        FailureClass::Authorization => "authorization",
        FailureClass::Protocol => "protocol",
        FailureClass::Transient => "transient",
        FailureClass::ResourceExpired => "resource-expired",
        FailureClass::RetryExhausted => "retry-exhausted",
    }
}

fn topic_drop_reason_name(reason: TopicDropReason) -> &'static str {
    match reason {
        TopicDropReason::InvalidPayload => "invalid-payload",
        TopicDropReason::NotRegistered => "not-registered",
        TopicDropReason::ChannelUnavailable => "channel-unavailable",
        TopicDropReason::QueueFull => "queue-full",
        TopicDropReason::Superseded => "superseded",
        TopicDropReason::HostRejected => "host-rejected",
        TopicDropReason::ChannelClosed => "channel-closed",
        TopicDropReason::TransportReplaced => "transport-replaced",
        TopicDropReason::SequenceExhausted => "sequence-exhausted",
    }
}

fn call_listener(listener: &Function, value: &JsValue, kind: &str) {
    if let Err(error) = listener.call1(&JsValue::UNDEFINED, value) {
        web_sys::console::error_2(
            &JsValue::from_str(&format!("browser {kind} listener threw")),
            &error,
        );
    }
}

fn set(object: &Object, name: &str, value: impl Into<JsValue>) {
    let result = Reflect::set(object, &JsValue::from_str(name), &value.into());
    debug_assert!(
        result.is_ok(),
        "plain object property assignment must succeed"
    );
}

fn js_error(message: impl AsRef<str>) -> JsValue {
    js_sys::Error::new(message.as_ref()).into()
}

fn js_message(value: JsValue) -> String {
    value
        .dyn_ref::<js_sys::Error>()
        .map(js_sys::Error::message)
        .and_then(|message| message.as_string())
        .or_else(|| value.as_string())
        .unwrap_or_else(|| "browser operation failed".into())
}
