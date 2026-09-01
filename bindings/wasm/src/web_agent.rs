use std::collections::{BTreeMap, BTreeSet, VecDeque};

use agent_core::{
    Agent, AgentConfig, AgentEffect, AgentError, AgentEvent, AgentNotification, AgentSnapshot,
    AudioIntent, AudioPreset, ClientConnectionState, ClientState, ConnectionIdentity,
    ConnectionPhase, EventDisposition, LatencyIntent, LocalAudioIntent, LocalSlotIntent,
    LocalVideoIntent, MediaKind, MetadataEntry, OrderedDelivery, Publication, SubscriptionIntent,
    TopicDelivery, TopicDirection, TopicError, TopicKind, TopicRegistration, Topology, UpstreamSlot,
    VideoPreset, VideoRequest,
};
use serde::Deserialize;
use wasm_bindgen::{closure::Closure, prelude::*, JsCast};

use crate::{mpsc, watch};

pub const TOPIC_BUFFERED_AMOUNT_LIMIT: u64 = 1_048_576;
pub const TOPIC_PENDING_BYTES_LIMIT: usize = 1_048_576;

// wasm-pack appends this verbatim to the generated .d.ts. Runtime classes are
// implemented below in Rust; this is only their precise TypeScript contract.
#[wasm_bindgen(typescript_custom_section)]
const TYPESCRIPT: &'static str = r#"
export interface Store<T> {
  getSnapshot(): T;
  subscribe(listener: () => void): () => void;
}

export type ConnectionPhase =
  | "disconnected"
  | "connecting"
  | "connected"
  | "reconnecting"
  | "failed";

export type AudioPreset = "speech" | "music";
export type VideoPreset = "motion" | "detail";
export type TopicKind = "latest" | "ordered";

export type VideoReceiveSlotCount = 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7;
export type AudioReceiveSlotCount = 0 | 1 | 2 | 3;
export type UpstreamSlotList =
  | readonly []
  | readonly [string]
  | readonly [string, string];

export interface WebAgentTopology<Slots extends UpstreamSlotList = UpstreamSlotList> {
  readonly upstreamSlots: Slots;
  readonly videoReceiveSlots: VideoReceiveSlotCount;
  readonly audioReceiveSlots: AudioReceiveSlotCount;
}

export interface WebAgentConfig<Slots extends UpstreamSlotList = UpstreamSlotList> {
  readonly endpoint: string;
  readonly topology: WebAgentTopology<Slots>;
}

export interface ConnectionIntent {
  readonly room: string;
  readonly token?: string;
  readonly metadata?: Readonly<Record<string, string>>;
}

export interface LocalAudioTrackIntent {
  readonly track: MediaStreamTrack;
  readonly muted?: boolean;
  readonly preset?: AudioPreset;
}

export interface LocalVideoTrackIntent {
  readonly track: MediaStreamTrack;
  readonly muted?: boolean;
  readonly preset?: VideoPreset;
}

export type LocalAudioIntent = MediaStreamTrack | LocalAudioTrackIntent | null;
export type LocalVideoIntent = MediaStreamTrack | LocalVideoTrackIntent | null;

export interface LocalSlotIntent {
  readonly audio?: LocalAudioIntent;
  readonly video?: LocalVideoIntent;
}

export type LocalMediaIntent<Slot extends string = string> = Readonly<
  Partial<Record<Slot, LocalSlotIntent>>
>;

export interface AdaptiveLatencyIntent {
  readonly mode: "adaptive";
}

export interface FixedLatencyIntent {
  readonly mode: "fixed";
  readonly minMs: number;
  readonly maxMs: number;
}

export type LatencyIntent = AdaptiveLatencyIntent | FixedLatencyIntent;

export interface RemoteVideoTrackState {
  /** Desired decoded/rendered height in physical pixels. */
  readonly targetHeight: number;
  /** Lowest useful height before this stream may be dropped. Defaults to 0. */
  readonly minHeight?: number;
  /** Lowest useful frame rate. Defaults to 0. */
  readonly minFps?: number;
  /** Higher values retain/gain quality first. Defaults to 0. */
  readonly priority?: number;
}

export interface RemoteVideoTrackSnapshot {
  readonly available: boolean;
  readonly mid: string | null;
  readonly paused: boolean;
  readonly mediaStreamTrack: MediaStreamTrack | null;
}

export interface RemoteAudioTrackSnapshot {
  readonly available: boolean;
  readonly mid: string | null;
  readonly levelDbov: number | null;
  readonly mediaStreamTrack: MediaStreamTrack | null;
}

export interface RemoteVideoTrack extends Store<RemoteVideoTrackSnapshot> {
  readonly id: string;
  readonly participantId: string;
  readonly kind: "video";
  getSnapshot(): RemoteVideoTrackSnapshot;
  subscribe(listener: () => void): () => void;
  setState(state: RemoteVideoTrackState | null): void;
}

export interface RemoteAudioTrack extends Store<RemoteAudioTrackSnapshot> {
  readonly id: string;
  readonly participantId: string;
  readonly kind: "audio";
  getSnapshot(): RemoteAudioTrackSnapshot;
  subscribe(listener: () => void): () => void;
}

export interface AudioSubscriptionIntent {
  /** Automatically select audible tracks. Defaults to true. */
  readonly auto?: boolean;
  /** Tracks to retain preferentially. */
  readonly pinned?: readonly RemoteAudioTrack[];
}

export interface LatestTopicMessage {
  readonly data: Uint8Array;
}

export interface OrderedTopicMessage {
  readonly data: Uint8Array;
  readonly publisherId: string;
  /** Decimal u64 string, preserving full JS precision. */
  readonly streamId: string;
  /** Decimal u64 string, preserving full JS precision. */
  readonly sequence: string;
}

export interface OrderedTopicResync {
  readonly publisherId: string;
  /** Decimal u64 string identifying the new ordered stream generation. */
  readonly streamId: string;
}

export interface LatestTopic {
  readonly name: string;
  readonly kind: "latest";
  /** Send through an already-desired publisher registration. Never opens the topic. */
  send(data: BufferSource): Promise<void>;
  onMessage(listener: (message: LatestTopicMessage) => void): () => void;
}

export interface OrderedTopic {
  readonly name: string;
  readonly kind: "ordered";
  /** Send through an already-desired publisher registration. Never opens the topic. */
  send(data: BufferSource): Promise<void>;
  onMessage(listener: (message: OrderedTopicMessage) => void): () => void;
  onResync(listener: (event: OrderedTopicResync) => void): () => void;
}

export type Topic = LatestTopic | OrderedTopic;

export interface LatestTopicPublisherScope {
  readonly publisherIds: readonly [string, ...string[]];
}

export interface LatestTopicIntent {
  readonly topic: LatestTopic;
  readonly publish?: boolean;
  readonly subscribe?: boolean | LatestTopicPublisherScope;
}

export interface OrderedTopicIntent {
  readonly topic: OrderedTopic;
  readonly publish?: boolean;
  readonly subscribe?: boolean;
}

export type TopicIntent = LatestTopicIntent | OrderedTopicIntent;

export interface WebAgentState<Slot extends string = string> {
  /** Omitted/null means disconnected. */
  readonly connection?: ConnectionIntent | null;
  /** Omitted means no local media attached. */
  readonly local?: LocalMediaIntent<Slot>;
  /** Omitted means automatic audio with no pins. */
  readonly audio?: AudioSubscriptionIntent;
  /** Omitted means adaptive latency. */
  readonly latency?: LatencyIntent;
  /** Omitted means no topic registrations are desired. */
  readonly topics?: readonly TopicIntent[];
}

export interface WebAgentSnapshot {
  readonly connection: ConnectionPhase;
  readonly participantId: string | null;
  readonly videoTracks: readonly RemoteVideoTrack[];
  readonly audioTracks: readonly RemoteAudioTrack[];
}

export type WebAgentErrorKind = "state" | "protocol" | "terminal" | "topic" | "bridge";

export interface WebAgentError {
  readonly kind: WebAgentErrorKind;
  readonly message: string;
}

export class WebAgent<const Slots extends UpstreamSlotList = UpstreamSlotList>
  implements Store<WebAgentSnapshot> {
  constructor(config: WebAgentConfig<Slots>);

  /** Replace the complete desired state. This is not a patch operation. */
  setState(state: WebAgentState<Slots[number]>): void;

  getSnapshot(): WebAgentSnapshot;
  subscribe(listener: () => void): () => void;
  onError(listener: (error: WebAgentError) => void): () => void;

  /** Return a stable inert logical-topic handle. This does not touch the network. */
  topic(name: string, options: { readonly kind: "latest" }): LatestTopic;
  topic(name: string, options: { readonly kind: "ordered" }): OrderedTopic;

  /** Terminal graceful shutdown. */
  close(): void;
}
"#;

#[wasm_bindgen(inline_js = r#"
export function __pulsebeam_make_topic(owner, name, kind, send) {
  const messages = new Set();
  const resyncs = new Set();
  const handle = {
    name,
    kind,
    send(data) { return send(data); },
    onMessage(listener) {
      if (typeof listener !== "function") throw new TypeError("listener must be a function");
      messages.add(listener);
      return () => messages.delete(listener);
    },
  };
  if (kind === "ordered") {
    handle.onResync = (listener) => {
      if (typeof listener !== "function") throw new TypeError("listener must be a function");
      resyncs.add(listener);
      return () => resyncs.delete(listener);
    };
  }
  Object.defineProperties(handle, {
    __pulsebeamOwner: { value: owner },
    __pulsebeamEmitMessage: { value(message) { for (const listener of [...messages]) listener(message); } },
    __pulsebeamEmitResync: { value(event) { for (const listener of [...resyncs]) listener(event); } },
  });
  return Object.freeze(handle);
}

export function __pulsebeam_make_remote_video(owner, id, participantId, snapshot, setState) {
  const listeners = new Set();
  let current = snapshot;
  const handle = {
    id,
    participantId,
    kind: "video",
    getSnapshot() { return current; },
    subscribe(listener) {
      if (typeof listener !== "function") throw new TypeError("listener must be a function");
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
    setState(state) {
      const error = setState(state);
      if (error instanceof Error) throw error;
    },
  };
  Object.defineProperties(handle, {
    __pulsebeamOwner: { value: owner },
    __pulsebeamUpdate: {
      value(next) {
        if (Object.is(current, next)) return;
        current = next;
        for (const listener of [...listeners]) listener();
      },
    },
  });
  return Object.freeze(handle);
}

export function __pulsebeam_make_remote_audio(owner, id, participantId, snapshot) {
  const listeners = new Set();
  let current = snapshot;
  const handle = {
    id,
    participantId,
    kind: "audio",
    getSnapshot() { return current; },
    subscribe(listener) {
      if (typeof listener !== "function") throw new TypeError("listener must be a function");
      listeners.add(listener);
      return () => listeners.delete(listener);
    },
  };
  Object.defineProperties(handle, {
    __pulsebeamOwner: { value: owner },
    __pulsebeamUpdate: {
      value(next) {
        if (Object.is(current, next)) return;
        current = next;
        for (const listener of [...listeners]) listener();
      },
    },
  });
  return Object.freeze(handle);
}

export function __pulsebeam_update_store(handle, snapshot) {
  handle.__pulsebeamUpdate(snapshot);
}
export function __pulsebeam_emit_topic_message(handle, message) {
  handle.__pulsebeamEmitMessage(message);
}
export function __pulsebeam_emit_topic_resync(handle, event) {
  handle.__pulsebeamEmitResync(event);
}
"#)]
extern "C" {
    fn __pulsebeam_make_topic(
        owner: &JsValue,
        name: &str,
        kind: &str,
        send: &js_sys::Function,
    ) -> JsValue;
    fn __pulsebeam_make_remote_video(
        owner: &JsValue,
        id: &str,
        participant_id: &str,
        snapshot: &JsValue,
        set_state: &js_sys::Function,
    ) -> JsValue;
    fn __pulsebeam_make_remote_audio(
        owner: &JsValue,
        id: &str,
        participant_id: &str,
        snapshot: &JsValue,
    ) -> JsValue;
    fn __pulsebeam_update_store(handle: &JsValue, snapshot: &JsValue);
    fn __pulsebeam_emit_topic_message(handle: &JsValue, message: &JsValue);
    fn __pulsebeam_emit_topic_resync(handle: &JsValue, event: &JsValue);
}

// -----------------------------------------------------------------------------
// Small JavaScript-facing reactive primitives
// -----------------------------------------------------------------------------

fn subscribe_listener(listeners: &js_sys::Array, listener: js_sys::Function) -> js_sys::Function {
    listeners.push(&listener);
    let listeners = listeners.clone();
    let listener = listener.clone();
    Closure::once_into_js(move || {
        for index in (0..listeners.length()).rev() {
            if listeners.get(index).strict_eq(listener.as_ref()) {
                let empty: [JsValue; 0] = [];
                let _ = listeners.splice_many(index, 1, &empty);
            }
        }
    })
    .unchecked_into()
}

fn notify(listeners: &js_sys::Array) {
    let snapshot = listeners.slice(0, listeners.length());
    for value in snapshot.iter() {
        if let Some(listener) = value.dyn_ref::<js_sys::Function>() {
            let _ = listener.call0(&JsValue::UNDEFINED);
        }
    }
}

fn emit(listeners: &js_sys::Array, detail: &JsValue) {
    let snapshot = listeners.slice(0, listeners.length());
    for value in snapshot.iter() {
        if let Some(listener) = value.dyn_ref::<js_sys::Function>() {
            let _ = listener.call1(&JsValue::UNDEFINED, detail);
        }
    }
}

fn set_js(object: &js_sys::Object, field: &str, value: JsValue) -> Result<(), JsValue> {
    js_sys::Reflect::set(object, &JsValue::from_str(field), &value).map(|_| ())
}

fn get_js(object: &JsValue, field: &str) -> Result<JsValue, JsValue> {
    js_sys::Reflect::get(object, &JsValue::from_str(field))
}

fn js_error(message: impl AsRef<str>) -> JsValue {
    js_sys::Error::new(message.as_ref()).into()
}

fn js_message(value: &JsValue) -> String {
    if let Some(error) = value.dyn_ref::<js_sys::Error>() {
        return String::from(error.message());
    }
    value
        .as_string()
        .unwrap_or_else(|| String::from("unknown JavaScript bridge error"))
}

fn connection_phase(value: &ConnectionPhase) -> &'static str {
    match value {
        ConnectionPhase::Disconnected => "disconnected",
        ConnectionPhase::Connecting => "connecting",
        ConnectionPhase::Connected => "connected",
        ConnectionPhase::Reconnecting => "reconnecting",
        ConnectionPhase::Failed => "failed",
    }
}

fn media_kind(value: MediaKind) -> &'static str {
    match value {
        MediaKind::Audio => "audio",
        MediaKind::Video => "video",
    }
}

fn topic_kind_name(value: TopicKind) -> &'static str {
    match value {
        TopicKind::Latest => "latest",
        TopicKind::Ordered => "ordered",
    }
}

fn parse_topic_kind(value: &str) -> Result<TopicKind, JsValue> {
    match value {
        "latest" => Ok(TopicKind::Latest),
        "ordered" => Ok(TopicKind::Ordered),
        other => Err(js_error(format!(
            "invalid topic kind {other:?}; expected \"latest\" or \"ordered\""
        ))),
    }
}

fn copy_buffer_source(value: JsValue) -> Result<Vec<u8>, JsValue> {
    if value.is_instance_of::<js_sys::ArrayBuffer>() {
        return Ok(js_sys::Uint8Array::new(&value).to_vec());
    }
    if !value.is_object() {
        return Err(js_error("topic payload must be a BufferSource"));
    }

    let buffer = get_js(&value, "buffer")?;
    if !buffer.is_instance_of::<js_sys::ArrayBuffer>() {
        return Err(js_error("topic payload must be a BufferSource"));
    }
    let byte_offset = get_js(&value, "byteOffset")?
        .as_f64()
        .ok_or_else(|| js_error("BufferSource.byteOffset must be a number"))?;
    let byte_length = get_js(&value, "byteLength")?
        .as_f64()
        .ok_or_else(|| js_error("BufferSource.byteLength must be a number"))?;
    if byte_offset < 0.0 || byte_length < 0.0 {
        return Err(js_error("invalid BufferSource bounds"));
    }
    let start = u32::try_from(byte_offset as u64).map_err(|_| js_error("BufferSource is too large"))?;
    let len = u32::try_from(byte_length as u64).map_err(|_| js_error("BufferSource is too large"))?;
    let end = start.checked_add(len).ok_or_else(|| js_error("BufferSource is too large"))?;
    Ok(js_sys::Uint8Array::new(&buffer).subarray(start, end).to_vec())
}

fn non_negative_u32(value: &JsValue, path: &str, default: u32) -> Result<u32, JsValue> {
    if value.is_null() || value.is_undefined() {
        return Ok(default);
    }
    let Some(number) = value.as_f64() else {
        return Err(js_error(format!("{path} must be a number")));
    };
    if !number.is_finite() || number < 0.0 || number.fract() != 0.0 || number > u32::MAX as f64 {
        return Err(js_error(format!("{path} must be a non-negative integer")));
    }
    Ok(number as u32)
}

// -----------------------------------------------------------------------------
// Public WebAgent
// -----------------------------------------------------------------------------

#[wasm_bindgen(skip_typescript)]
pub struct WebAgent {
    owner: JsValue,
    slots: BTreeSet<String>,
    desired: watch::Sender<DesiredState>,
    commands: mpsc::Sender<DriverCommand>,
    snapshots: watch::Receiver<JsValue>,
    snapshot: JsValue,
    listeners: js_sys::Array,
    errors: js_sys::Array,
    topics: BTreeMap<String, TopicHandle>,
    closed: bool,
}

enum TopicHandle {
    Latest(JsValue),
    Ordered(JsValue),
}

impl TopicHandle {
    fn kind(&self) -> TopicKind {
        match self {
            Self::Latest(_) => TopicKind::Latest,
            Self::Ordered(_) => TopicKind::Ordered,
        }
    }

    fn js(&self) -> JsValue {
        match self {
            Self::Latest(value) | Self::Ordered(value) => value.clone(),
        }
    }
}

#[wasm_bindgen]
impl WebAgent {
    #[wasm_bindgen(constructor, skip_typescript)]
    pub fn new(config: JsValue) -> Result<WebAgent, JsValue> {
        let dto: WebAgentConfigDto = serde_wasm_bindgen::from_value(config)
            .map_err(|error| js_error(format!("invalid WebAgent config: {error}")))?;
        let slots: BTreeSet<_> = dto.topology.upstream_slots.iter().cloned().collect();
        if slots.len() != dto.topology.upstream_slots.len() {
            return Err(js_error("upstream slot names must be unique"));
        }
        if slots.iter().any(|slot| slot.is_empty()) {
            return Err(js_error("upstream slot names must not be empty"));
        }
        let video_receive_slots = dto.topology.video_receive_slots as usize;
        let core_config = dto.into_core()?;

        let owner: JsValue = js_sys::Object::new().into();
        let listeners = js_sys::Array::new();
        let errors = js_sys::Array::new();
        let initial_snapshot = empty_agent_snapshot();
        let (snapshots_tx, snapshots) = watch::channel::<JsValue>();
        let (desired, desired_rx) = watch::channel::<DesiredState>();
        let (commands, inbox) = mpsc::channel();

        let driver = Driver::new(
            core_config,
            video_receive_slots,
            owner.clone(),
            desired_rx,
            commands.clone(),
            snapshots_tx,
            listeners.clone(),
            errors.clone(),
        );
        wasm_bindgen_futures::spawn_local(driver.run(inbox));

        Ok(Self {
            owner,
            slots,
            desired,
            commands,
            snapshots,
            snapshot: initial_snapshot,
            listeners,
            errors,
            topics: BTreeMap::new(),
            closed: false,
        })
    }

    #[wasm_bindgen(js_name = setState, skip_typescript)]
    pub fn set_state(&mut self, state: JsValue) -> Result<(), JsValue> {
        self.assert_open()?;
        let state = DesiredState::from_js(state, &self.owner, &self.slots)?;
        self.desired
            .send(state)
            .map_err(|_| js_error("WebAgent has stopped"))?;
        self.commands
            .send(DriverCommand::DesiredChanged)
            .map_err(|_| js_error("WebAgent has stopped"))
    }

    #[wasm_bindgen(js_name = getSnapshot, skip_typescript)]
    pub fn get_snapshot(&mut self) -> JsValue {
        loop {
            match self.snapshots.try_recv() {
                Ok(snapshot) => self.snapshot = snapshot,
                Err(watch::TryRecvError::Empty | watch::TryRecvError::Closed) => break,
            }
        }
        self.snapshot.clone()
    }

    #[wasm_bindgen(skip_typescript)]
    pub fn subscribe(&self, listener: js_sys::Function) -> js_sys::Function {
        subscribe_listener(&self.listeners, listener)
    }

    #[wasm_bindgen(js_name = onError, skip_typescript)]
    pub fn on_error(&self, listener: js_sys::Function) -> js_sys::Function {
        subscribe_listener(&self.errors, listener)
    }

    #[wasm_bindgen(skip_typescript)]
    pub fn topic(&mut self, name: String, options: JsValue) -> Result<JsValue, JsValue> {
        self.assert_open()?;
        validate_topic_name(&name)?;
        let kind = get_js(&options, "kind")?
            .as_string()
            .ok_or_else(|| js_error("topic options.kind must be a string"))?;
        let kind = parse_topic_kind(&kind)?;

        if let Some(existing) = self.topics.get(&name) {
            if existing.kind() != kind {
                return Err(js_error(format!(
                    "topic {name:?} is already defined as {}",
                    topic_kind_name(existing.kind())
                )));
            }
            return Ok(existing.js());
        }

        let registration = TopicRegistration {
            topic: name.clone(),
            kind,
            direction: TopicDirection::Publish,
            publisher_id: None,
        };
        let send = topic_send_callback(self.commands.clone(), registration);
        let value = __pulsebeam_make_topic(
            &self.owner,
            &name,
            topic_kind_name(kind),
            &send,
        );

        self.commands
            .send(DriverCommand::RegisterTopicSink {
                name: name.clone(),
                kind,
                handle: value.clone(),
            })
            .map_err(|_| js_error("WebAgent has stopped"))?;

        self.topics.insert(
            name,
            match kind {
                TopicKind::Latest => TopicHandle::Latest(value.clone()),
                TopicKind::Ordered => TopicHandle::Ordered(value.clone()),
            },
        );
        Ok(value)
    }

    #[wasm_bindgen(skip_typescript)]
    pub fn close(&mut self) -> Result<(), JsValue> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.commands
            .send(DriverCommand::Close)
            .map_err(|_| js_error("WebAgent has stopped"))
    }
}

impl WebAgent {
    fn assert_open(&self) -> Result<(), JsValue> {
        if self.closed {
            Err(js_error("WebAgent is closed"))
        } else {
            Ok(())
        }
    }
}

impl Drop for WebAgent {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.commands.send(DriverCommand::Abort);
        }
    }
}

// -----------------------------------------------------------------------------
// Stable JS handle callbacks.
// -----------------------------------------------------------------------------

fn topic_send_callback(
    commands: mpsc::Sender<DriverCommand>,
    registration: TopicRegistration,
) -> js_sys::Function {
    Closure::<dyn FnMut(JsValue) -> js_sys::Promise>::new(move |data: JsValue| {
        let payload = copy_buffer_source(data);
        let commands = commands.clone();
        let registration = registration.clone();
        wasm_bindgen_futures::future_to_promise(async move {
            let payload = payload?;
            send_topic(&commands, registration, payload).await?;
            Ok(JsValue::UNDEFINED)
        })
    })
    .into_js_value()
    .unchecked_into()
}

async fn send_topic(
    commands: &mpsc::Sender<DriverCommand>,
    registration: TopicRegistration,
    payload: Vec<u8>,
) -> Result<(), JsValue> {
    let (reply, mut response) = mpsc::channel();
    commands
        .send(DriverCommand::SendTopic {
            registration,
            payload,
            reply,
        })
        .map_err(|_| js_error("WebAgent has stopped"))?;
    response
        .recv()
        .await
        .map_err(|_| js_error("WebAgent has stopped"))?
        .map_err(js_error)
}

fn remote_video_set_state_callback(
    commands: mpsc::Sender<DriverCommand>,
    track_id: String,
) -> js_sys::Function {
    Closure::<dyn FnMut(JsValue) -> JsValue>::new(move |state: JsValue| {
        let state = match parse_video_state(state, &track_id) {
            Ok(state) => state,
            Err(error) => return error,
        };
        match commands.send(DriverCommand::VideoIntentChanged {
            track_id: track_id.clone(),
            state,
        }) {
            Ok(()) => JsValue::UNDEFINED,
            Err(_) => js_sys::Error::new("WebAgent has stopped").into(),
        }
    })
    .into_js_value()
    .unchecked_into()
}

fn parse_video_state(value: JsValue, track_id: &str) -> Result<Option<VideoRequest>, JsValue> {
    if value.is_null() || value.is_undefined() {
        return Ok(None);
    }
    if !value.is_object() {
        return Err(js_error("RemoteVideoTrack state must be an object or null"));
    }
    let target_height = non_negative_u32(&get_js(&value, "targetHeight")?, "targetHeight", 0)?;
    let min_height = non_negative_u32(&get_js(&value, "minHeight")?, "minHeight", 0)?;
    let min_fps = non_negative_u32(&get_js(&value, "minFps")?, "minFps", 0)?;
    let priority = non_negative_u32(&get_js(&value, "priority")?, "priority", 0)?;
    if min_height > target_height {
        return Err(js_error("minHeight must not exceed targetHeight"));
    }
    Ok(Some(VideoRequest {
        track_id: String::from(track_id),
        target_height,
        min_height,
        min_fps,
        priority,
    }))
}

// -----------------------------------------------------------------------------
// Web-owned input parsing. agent_core itself has no Serde requirement.
// -----------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebAgentConfigDto {
    endpoint: String,
    topology: WebTopologyConfigDto,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebTopologyConfigDto {
    #[serde(default)]
    upstream_slots: Vec<String>,
    #[serde(default)]
    video_receive_slots: u8,
    #[serde(default)]
    audio_receive_slots: u8,
}

impl WebAgentConfigDto {
    fn into_core(self) -> Result<AgentConfig, JsValue> {
        let upstream = self
            .topology
            .upstream_slots
            .into_iter()
            .map(UpstreamSlot::new)
            .collect();
        let topology = Topology::new(
            upstream,
            self.topology.video_receive_slots,
            self.topology.audio_receive_slots,
        )
        .map_err(|error| js_error(format!("invalid WebAgent topology: {error}")))?;
        AgentConfig::new(self.endpoint, topology)
            .map_err(|error| js_error(format!("invalid WebAgent config: {error}")))
    }
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct WebStateDto {
    #[serde(default)]
    connection: Option<WebConnectionDto>,
    #[serde(default)]
    latency: WebLatencyDto,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebConnectionDto {
    room: String,
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
}

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase")]
enum WebLatencyDto {
    Adaptive,
    Fixed {
        #[serde(rename = "minMs")]
        min_ms: u32,
        #[serde(rename = "maxMs")]
        max_ms: u32,
    },
}

impl Default for WebLatencyDto {
    fn default() -> Self {
        Self::Adaptive
    }
}

impl WebLatencyDto {
    fn into_core(self) -> LatencyIntent {
        match self {
            Self::Adaptive => LatencyIntent::Adaptive,
            Self::Fixed { min_ms, max_ms } => LatencyIntent::Fixed { min_ms, max_ms },
        }
    }
}

#[derive(Default)]
struct DesiredState {
    client: ClientState,
    local_tracks: Vec<LocalSlotTracks>,
}

#[derive(Clone)]
struct LocalSlotTracks {
    slot: String,
    audio: Option<web_sys::MediaStreamTrack>,
    video: Option<web_sys::MediaStreamTrack>,
}

impl DesiredState {
    fn from_js(value: JsValue, owner: &JsValue, slots: &BTreeSet<String>) -> Result<Self, JsValue> {
        if value.is_null() || value.is_undefined() || !value.is_object() {
            return Err(js_error("WebAgent state must be an object"));
        }

        // Serde sees only plain web DTO fields. Browser handles are parsed below.
        let dto: WebStateDto = serde_wasm_bindgen::from_value(value.clone())
            .map_err(|error| js_error(format!("invalid WebAgent state: {error}")))?;
        let (local_slots, local_tracks) = parse_local_slots(&value, slots)?;
        let audio = parse_audio_intent(&value, owner)?;
        let mut topics = parse_topic_intents(&value, owner)?;
        topics.sort();

        let identity = dto.connection.map(|connection| ConnectionIdentity {
            room: connection.room,
            token: connection.token,
            metadata: connection
                .metadata
                .into_iter()
                .map(|(name, value)| MetadataEntry { name, value })
                .collect(),
        });

        Ok(Self {
            client: ClientState {
                connection: if identity.is_some() {
                    ClientConnectionState::Connected
                } else {
                    ClientConnectionState::Disconnected
                },
                identity,
                local_slots,
                subscriptions: SubscriptionIntent {
                    video: Vec::new(),
                    audio,
                },
                latency: dto.latency.into_core(),
                topics,
            },
            local_tracks,
        })
    }
}

fn parse_local_slots(
    state: &JsValue,
    configured_slots: &BTreeSet<String>,
) -> Result<(Vec<LocalSlotIntent>, Vec<LocalSlotTracks>), JsValue> {
    let local = get_js(state, "local")?;
    if local.is_null() || local.is_undefined() {
        return Ok((Vec::new(), Vec::new()));
    }
    if !local.is_object() {
        return Err(js_error("state.local must be an object keyed by upstream slot"));
    }

    let local = js_sys::Object::from(local);
    let keys = js_sys::Object::keys(&local);
    let mut intents = Vec::with_capacity(keys.length() as usize);
    let mut tracks = Vec::with_capacity(keys.length() as usize);

    for key in keys.iter() {
        let Some(slot) = key.as_string() else {
            continue;
        };
        if !configured_slots.contains(&slot) {
            return Err(js_error(format!("unknown upstream slot {slot:?}")));
        }
        let slot_value = js_sys::Reflect::get(&local, &key)?;
        if slot_value.is_null() || !slot_value.is_object() {
            return Err(js_error(format!("state.local.{slot} must be an object")));
        }

        let (audio, audio_track) = parse_local_audio(&slot_value, &slot)?;
        let (video, video_track) = parse_local_video(&slot_value, &slot)?;
        intents.push(LocalSlotIntent {
            slot: slot.clone(),
            audio,
            video,
        });
        tracks.push(LocalSlotTracks {
            slot,
            audio: audio_track,
            video: video_track,
        });
    }
    intents.sort_by(|left, right| left.slot.cmp(&right.slot));
    tracks.sort_by(|left, right| left.slot.cmp(&right.slot));
    Ok((intents, tracks))
}

fn parse_local_audio(
    slot_value: &JsValue,
    slot: &str,
) -> Result<(LocalAudioIntent, Option<web_sys::MediaStreamTrack>), JsValue> {
    let value = get_js(slot_value, "audio")?;
    if value.is_null() || value.is_undefined() {
        return Ok((LocalAudioIntent::default(), None));
    }
    if value.is_instance_of::<web_sys::MediaStreamTrack>() {
        let track = value
            .dyn_into::<web_sys::MediaStreamTrack>()
            .map_err(|_| js_error(format!("state.local.{slot}.audio is not a MediaStreamTrack")))?;
        return Ok((
            LocalAudioIntent {
                attached: true,
                muted: false,
                preset: AudioPreset::Speech,
            },
            Some(track),
        ));
    }
    if !value.is_object() {
        return Err(js_error(format!(
            "state.local.{slot}.audio must be a MediaStreamTrack, media config, or null"
        )));
    }
    let track = optional_track_property(&value, "track", &format!("state.local.{slot}.audio.track"))?;
    let muted = optional_bool_property(&value, "muted", false, &format!("state.local.{slot}.audio.muted"))?;
    let preset = match optional_string_property(&value, "preset", "speech", &format!("state.local.{slot}.audio.preset"))?.as_str() {
        "speech" => AudioPreset::Speech,
        "music" => AudioPreset::Music,
        other => return Err(js_error(format!(
            "state.local.{slot}.audio.preset must be \"speech\" or \"music\", got {other:?}"
        ))),
    };
    Ok((
        LocalAudioIntent {
            attached: track.is_some(),
            muted,
            preset,
        },
        track,
    ))
}

fn parse_local_video(
    slot_value: &JsValue,
    slot: &str,
) -> Result<(LocalVideoIntent, Option<web_sys::MediaStreamTrack>), JsValue> {
    let value = get_js(slot_value, "video")?;
    if value.is_null() || value.is_undefined() {
        return Ok((LocalVideoIntent::default(), None));
    }
    if value.is_instance_of::<web_sys::MediaStreamTrack>() {
        let track = value
            .dyn_into::<web_sys::MediaStreamTrack>()
            .map_err(|_| js_error(format!("state.local.{slot}.video is not a MediaStreamTrack")))?;
        return Ok((
            LocalVideoIntent {
                attached: true,
                muted: false,
                preset: VideoPreset::Motion,
            },
            Some(track),
        ));
    }
    if !value.is_object() {
        return Err(js_error(format!(
            "state.local.{slot}.video must be a MediaStreamTrack, media config, or null"
        )));
    }
    let track = optional_track_property(&value, "track", &format!("state.local.{slot}.video.track"))?;
    let muted = optional_bool_property(&value, "muted", false, &format!("state.local.{slot}.video.muted"))?;
    let preset = match optional_string_property(&value, "preset", "motion", &format!("state.local.{slot}.video.preset"))?.as_str() {
        "motion" => VideoPreset::Motion,
        "detail" => VideoPreset::Detail,
        other => return Err(js_error(format!(
            "state.local.{slot}.video.preset must be \"motion\" or \"detail\", got {other:?}"
        ))),
    };
    Ok((
        LocalVideoIntent {
            attached: track.is_some(),
            muted,
            preset,
        },
        track,
    ))
}

fn optional_track_property(
    object: &JsValue,
    field: &str,
    path: &str,
) -> Result<Option<web_sys::MediaStreamTrack>, JsValue> {
    let value = get_js(object, field)?;
    if value.is_null() || value.is_undefined() {
        return Ok(None);
    }
    value
        .dyn_into::<web_sys::MediaStreamTrack>()
        .map(Some)
        .map_err(|_| js_error(format!("{path} must be a MediaStreamTrack or null")))
}

fn optional_bool_property(
    object: &JsValue,
    field: &str,
    default: bool,
    path: &str,
) -> Result<bool, JsValue> {
    let value = get_js(object, field)?;
    if value.is_null() || value.is_undefined() {
        return Ok(default);
    }
    value
        .as_bool()
        .ok_or_else(|| js_error(format!("{path} must be a boolean")))
}

fn optional_string_property(
    object: &JsValue,
    field: &str,
    default: &str,
    path: &str,
) -> Result<String, JsValue> {
    let value = get_js(object, field)?;
    if value.is_null() || value.is_undefined() {
        return Ok(String::from(default));
    }
    value
        .as_string()
        .ok_or_else(|| js_error(format!("{path} must be a string")))
}

fn parse_audio_intent(state: &JsValue, owner: &JsValue) -> Result<AudioIntent, JsValue> {
    let audio = get_js(state, "audio")?;
    if audio.is_null() || audio.is_undefined() {
        return Ok(AudioIntent {
            pinned: Vec::new(),
            auto: true,
        });
    }
    if !audio.is_object() {
        return Err(js_error("state.audio must be an object"));
    }

    let auto = optional_bool_property(&audio, "auto", true, "state.audio.auto")?;
    let pinned = get_js(&audio, "pinned")?;
    let mut ids = Vec::new();
    if !pinned.is_null() && !pinned.is_undefined() {
        if !js_sys::Array::is_array(&pinned) {
            return Err(js_error("state.audio.pinned must be an array"));
        }
        for track in js_sys::Array::from(&pinned).iter() {
            assert_owned_handle(&track, owner, "state.audio.pinned")?;
            let id = get_js(&track, "id")?
                .as_string()
                .ok_or_else(|| js_error("state.audio.pinned contains an invalid track"))?;
            ids.push(id);
        }
    }
    Ok(AudioIntent { pinned: ids, auto })
}

fn parse_topic_intents(state: &JsValue, owner: &JsValue) -> Result<Vec<TopicRegistration>, JsValue> {
    let topics = get_js(state, "topics")?;
    if topics.is_null() || topics.is_undefined() {
        return Ok(Vec::new());
    }
    if !js_sys::Array::is_array(&topics) {
        return Err(js_error("state.topics must be an array"));
    }

    let mut registrations = Vec::new();
    let mut seen = BTreeSet::new();
    for value in js_sys::Array::from(&topics).iter() {
        if !value.is_object() {
            return Err(js_error("state.topics entries must be objects"));
        }
        let topic = get_js(&value, "topic")?;
        assert_owned_handle(&topic, owner, "state.topics[].topic")?;
        let name = get_js(&topic, "name")?
            .as_string()
            .ok_or_else(|| js_error("topic handle has no name"))?;
        let kind_name = get_js(&topic, "kind")?
            .as_string()
            .ok_or_else(|| js_error("topic handle has no kind"))?;
        let kind = parse_topic_kind(&kind_name)?;
        if !seen.insert(name.clone()) {
            return Err(js_error(format!("duplicate topic intent for {name:?}")));
        }

        let publish = optional_bool_property(&value, "publish", false, "state.topics[].publish")?;
        if publish {
            registrations.push(TopicRegistration {
                topic: name.clone(),
                kind,
                direction: TopicDirection::Publish,
                publisher_id: None,
            });
        }

        let subscribe = get_js(&value, "subscribe")?;
        match kind {
            TopicKind::Ordered => {
                if subscribe.is_null() || subscribe.is_undefined() || subscribe == JsValue::FALSE {
                    continue;
                }
                if subscribe != JsValue::TRUE {
                    return Err(js_error("ordered topic subscribe must be a boolean"));
                }
                registrations.push(TopicRegistration {
                    topic: name,
                    kind,
                    direction: TopicDirection::Subscribe,
                    publisher_id: None,
                });
            }
            TopicKind::Latest => {
                if subscribe.is_null() || subscribe.is_undefined() || subscribe == JsValue::FALSE {
                    continue;
                }
                if subscribe == JsValue::TRUE {
                    registrations.push(TopicRegistration {
                        topic: name,
                        kind,
                        direction: TopicDirection::Subscribe,
                        publisher_id: None,
                    });
                    continue;
                }
                if !subscribe.is_object() {
                    return Err(js_error(
                        "latest topic subscribe must be a boolean or { publisherIds }",
                    ));
                }
                let publisher_ids = get_js(&subscribe, "publisherIds")?;
                if !js_sys::Array::is_array(&publisher_ids) {
                    return Err(js_error("latest topic publisherIds must be an array"));
                }
                let publisher_ids = js_sys::Array::from(&publisher_ids);
                if publisher_ids.length() == 0 {
                    return Err(js_error("latest topic publisherIds must not be empty"));
                }
                let mut unique = BTreeSet::new();
                for publisher_id in publisher_ids.iter() {
                    let publisher_id = publisher_id
                        .as_string()
                        .ok_or_else(|| js_error("topic publisher id must be a string"))?;
                    if publisher_id.is_empty() {
                        return Err(js_error("topic publisher id must not be empty"));
                    }
                    if !unique.insert(publisher_id.clone()) {
                        return Err(js_error(format!(
                            "duplicate publisher scope {publisher_id:?} for topic {name:?}"
                        )));
                    }
                    registrations.push(TopicRegistration {
                        topic: name.clone(),
                        kind,
                        direction: TopicDirection::Subscribe,
                        publisher_id: Some(publisher_id),
                    });
                }
            }
        }
    }
    Ok(registrations)
}

fn assert_owned_handle(value: &JsValue, owner: &JsValue, path: &str) -> Result<(), JsValue> {
    if !value.is_object() {
        return Err(js_error(format!("{path} must contain a PulseBeam handle")));
    }
    let handle_owner = get_js(value, "__pulsebeamOwner")?;
    if !handle_owner.strict_eq(owner) {
        return Err(js_error(format!(
            "{path} contains a handle owned by another WebAgent"
        )));
    }
    Ok(())
}

fn validate_topic_name(name: &str) -> Result<(), JsValue> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(js_error(
            "topic names may contain only ASCII letters, digits, '-' and '_'",
        ));
    }
    Ok(())
}

fn same_desired_state(left: &DesiredState, right: &DesiredState) -> bool {
    left.client == right.client
        && left.local_tracks.len() == right.local_tracks.len()
        && left.local_tracks.iter().zip(&right.local_tracks).all(|(left, right)| {
            left.slot == right.slot && left.audio == right.audio && left.video == right.video
        })
}

// -----------------------------------------------------------------------------
// Driver: sole mutable owner of Agent + browser runtime
// -----------------------------------------------------------------------------

struct TopicSink {
    kind: TopicKind,
    handle: JsValue,
}

enum DriverCommand {
    DesiredChanged,
    VideoIntentChanged {
        track_id: String,
        state: Option<VideoRequest>,
    },
    RegisterTopicSink {
        name: String,
        kind: TopicKind,
        handle: JsValue,
    },
    CoreEvent(AgentEvent),
    RemoteTrack {
        generation: agent_core::Generation,
        mid: String,
        track: web_sys::MediaStreamTrack,
    },
    TimerFired {
        id: agent_core::TimerId,
    },
    HttpFinished {
        id: agent_core::RequestId,
        event: AgentEvent,
    },
    TopicWritable,
    SendTopic {
        registration: TopicRegistration,
        payload: Vec<u8>,
        reply: mpsc::Sender<Result<(), String>>,
    },
    Close,
    Abort,
}

struct PendingTopicSend {
    payload: Vec<u8>,
    reply: mpsc::Sender<Result<(), String>>,
}

struct Driver {
    core: Agent,
    runtime: BrowserRuntime,
    video_receive_slots: usize,
    owner: JsValue,
    desired_rx: watch::Receiver<DesiredState>,
    desired: DesiredState,
    video_intents: BTreeMap<String, VideoRequest>,
    commands: mpsc::Sender<DriverCommand>,
    snapshots: watch::Sender<JsValue>,
    listeners: js_sys::Array,
    errors: js_sys::Array,
    topic_sinks: BTreeMap<String, TopicSink>,
    remote: RemoteRegistry,
    pending_topic_sends: BTreeMap<TopicRegistration, VecDeque<PendingTopicSend>>,
    pending_topic_bytes: usize,
}

impl Driver {
    #[allow(clippy::too_many_arguments)]
    fn new(
        config: AgentConfig,
        video_receive_slots: usize,
        owner: JsValue,
        desired_rx: watch::Receiver<DesiredState>,
        commands: mpsc::Sender<DriverCommand>,
        snapshots: watch::Sender<JsValue>,
        listeners: js_sys::Array,
        errors: js_sys::Array,
    ) -> Self {
        Self {
            core: Agent::new(config),
            runtime: BrowserRuntime::new(),
            video_receive_slots,
            owner: owner.clone(),
            desired_rx,
            desired: DesiredState::default(),
            video_intents: BTreeMap::new(),
            commands: commands.clone(),
            snapshots,
            listeners,
            errors,
            topic_sinks: BTreeMap::new(),
            remote: RemoteRegistry::new(owner, commands),
            pending_topic_sends: BTreeMap::new(),
            pending_topic_bytes: 0,
        }
    }

    async fn run(mut self, mut inbox: mpsc::Receiver<DriverCommand>) {
        self.publish_state();
        let mut closing = false;
        let mut aborted = false;

        while let Ok(command) = inbox.recv().await {
            match command {
                DriverCommand::Abort => {
                    aborted = true;
                    break;
                }
                DriverCommand::Close if !closing => {
                    closing = true;
                    self.apply_close();
                }
                DriverCommand::Close => {}
                DriverCommand::HttpFinished { id, event } => {
                    self.runtime.request_finished(id);
                    if !closing {
                        self.feed(event);
                    }
                }
                DriverCommand::TimerFired { id } => {
                    self.runtime.timer_finished(id);
                    if !closing {
                        self.feed(AgentEvent::Timer(agent_core::TimerEvent::Fired { id }));
                    }
                }
                DriverCommand::DesiredChanged if !closing => self.apply_latest_desired(),
                DriverCommand::VideoIntentChanged { track_id, state } if !closing => {
                    let changed = if let Some(state) = state {
                        if !self.remote.is_available_video(&track_id) {
                            false
                        } else if self.video_intents.get(&track_id) == Some(&state) {
                            false
                        } else {
                            self.video_intents.insert(track_id, state);
                            true
                        }
                    } else {
                        self.video_intents.remove(&track_id).is_some()
                    };
                    if changed {
                        self.apply_resolved_desired();
                    }
                }
                DriverCommand::RegisterTopicSink {
                    name,
                    kind,
                    handle,
                } => {
                    self.topic_sinks.insert(name, TopicSink { kind, handle });
                }
                DriverCommand::CoreEvent(event) if !closing => self.feed(event),
                DriverCommand::RemoteTrack {
                    generation,
                    mid,
                    track,
                } if !closing => {
                    if self.remote.track(generation, mid, track) {
                        self.reconcile_remote();
                        self.publish_state();
                    }
                }
                DriverCommand::TopicWritable if !closing => {}
                DriverCommand::SendTopic {
                    registration,
                    payload,
                    reply,
                } if !closing => self.accept_topic_send(registration, payload, reply),
                DriverCommand::SendTopic { reply, .. } => {
                    let _ = reply.send(Err(String::from("WebAgent is closed")));
                }
                DriverCommand::DesiredChanged
                | DriverCommand::VideoIntentChanged { .. }
                | DriverCommand::CoreEvent(_)
                | DriverCommand::RemoteTrack { .. }
                | DriverCommand::TopicWritable => {}
            }

            if !closing {
                self.drain_pending_topic_sends();
            }
            if closing && self.runtime.graceful_close_ready() {
                break;
            }
        }

        self.reject_all_pending_topic_sends(if aborted {
            "WebAgent was aborted"
        } else {
            "WebAgent is closed"
        });
        if aborted {
            self.runtime.close_all();
        } else {
            self.runtime.finish_graceful_close();
        }
    }

    fn apply_latest_desired(&mut self) {
        let mut latest = None;
        loop {
            match self.desired_rx.try_recv() {
                Ok(state) => latest = Some(state),
                Err(watch::TryRecvError::Empty | watch::TryRecvError::Closed) => break,
            }
        }
        let Some(state) = latest else {
            return;
        };
        if same_desired_state(&self.desired, &state) {
            return;
        }
        self.runtime.set_local_tracks(state.local_tracks.clone());
        self.desired = state;
        self.apply_resolved_desired();
    }

    fn apply_resolved_desired(&mut self) {
        let mut state = self.desired.client.clone();
        let mut video: Vec<_> = self
            .video_intents
            .values()
            .filter(|request| self.remote.is_available_video(&request.track_id))
            .cloned()
            .collect();
        video.sort_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left.track_id.cmp(&right.track_id))
        });
        video.truncate(self.video_receive_slots);
        state.subscriptions.video = video;

        if let Err(error) = self.core.set_state(state) {
            self.error("state", error.to_string());
            return;
        }
        self.reject_undesired_pending_topic_sends();
        self.drain_core();
    }

    fn apply_close(&mut self) {
        self.reject_all_pending_topic_sends("WebAgent is closed");
        self.video_intents.clear();
        self.runtime.set_local_tracks(Vec::new());

        let mut state = self.core.desired_state().clone();
        state.connection = ClientConnectionState::Disconnected;
        state.identity = None;
        state.local_slots.clear();
        state.subscriptions = SubscriptionIntent::default();
        state.topics.clear();
        if let Err(error) = self.core.set_state(state) {
            self.error("state", error.to_string());
            return;
        }
        self.drain_core();
    }

    fn feed(&mut self, event: AgentEvent) {
        if self.core.handle(event) != EventDisposition::IgnoredStale {
            self.drain_core();
        }
    }

    fn accept_topic_send(
        &mut self,
        registration: TopicRegistration,
        payload: Vec<u8>,
        reply: mpsc::Sender<Result<(), String>>,
    ) {
        if let Err(error) = self.validate_topic_send(&registration) {
            let _ = reply.send(Err(error.to_string()));
            return;
        }
        if self.runtime.can_send(&registration, payload.len()) {
            let result = self
                .core
                .send_topic(&registration, payload)
                .map_err(|error| error.to_string());
            if result.is_ok() {
                self.drain_core();
            }
            let _ = reply.send(result);
            return;
        }
        if self.pending_topic_bytes.saturating_add(payload.len()) > TOPIC_PENDING_BYTES_LIMIT {
            let _ = reply.send(Err(String::from("topic send queue is full")));
            return;
        }
        self.pending_topic_bytes = self.pending_topic_bytes.saturating_add(payload.len());
        self.pending_topic_sends
            .entry(registration)
            .or_default()
            .push_back(PendingTopicSend { payload, reply });
    }

    fn validate_topic_send(&self, registration: &TopicRegistration) -> Result<(), TopicError> {
        if !self.core.desired_state().topics.contains(registration) {
            return Err(TopicError::Unregistered);
        }
        if registration.direction != TopicDirection::Publish {
            return Err(TopicError::NotPublisher);
        }
        Ok(())
    }

    fn drain_pending_topic_sends(&mut self) {
        let registrations: Vec<_> = self.pending_topic_sends.keys().cloned().collect();
        for registration in registrations {
            loop {
                let Some(payload_len) = self
                    .pending_topic_sends
                    .get(&registration)
                    .and_then(|queue| queue.front())
                    .map(|pending| pending.payload.len())
                else {
                    break;
                };
                if self.validate_topic_send(&registration).is_err() {
                    self.reject_pending_topic_registration(
                        &registration,
                        "topic is no longer desired for publishing",
                    );
                    break;
                }
                if !self.runtime.can_send(&registration, payload_len) {
                    break;
                }
                let Some(pending) = self
                    .pending_topic_sends
                    .get_mut(&registration)
                    .and_then(VecDeque::pop_front)
                else {
                    break;
                };
                self.pending_topic_bytes =
                    self.pending_topic_bytes.saturating_sub(pending.payload.len());
                let result = self
                    .core
                    .send_topic(&registration, pending.payload)
                    .map_err(|error| error.to_string());
                let sent = result.is_ok();
                let _ = pending.reply.send(result);
                if sent {
                    self.drain_core();
                }
            }
            if self
                .pending_topic_sends
                .get(&registration)
                .is_some_and(VecDeque::is_empty)
            {
                self.pending_topic_sends.remove(&registration);
            }
        }
    }

    fn reject_undesired_pending_topic_sends(&mut self) {
        let registrations: Vec<_> = self
            .pending_topic_sends
            .keys()
            .filter(|registration| self.validate_topic_send(registration).is_err())
            .cloned()
            .collect();
        for registration in registrations {
            self.reject_pending_topic_registration(
                &registration,
                "topic is no longer desired for publishing",
            );
        }
    }

    fn reject_pending_topic_registration(&mut self, registration: &TopicRegistration, reason: &str) {
        let Some(mut queue) = self.pending_topic_sends.remove(registration) else {
            return;
        };
        while let Some(pending) = queue.pop_front() {
            self.pending_topic_bytes =
                self.pending_topic_bytes.saturating_sub(pending.payload.len());
            let _ = pending.reply.send(Err(String::from(reason)));
        }
    }

    fn reject_all_pending_topic_sends(&mut self, reason: &str) {
        let registrations: Vec<_> = self.pending_topic_sends.keys().cloned().collect();
        for registration in registrations {
            self.reject_pending_topic_registration(&registration, reason);
        }
    }

    fn drain_core(&mut self) {
        let mut state_changed = false;
        while let Some(effect) = self.core.next_effect() {
            state_changed |= self.observe_transport_lifecycle(&effect);
            self.runtime.execute(effect, self.commands.clone());
        }
        state_changed |= self.reconcile_remote();

        while let Some(notification) = self.core.next_notification() {
            match notification {
                AgentNotification::Connection(_)
                | AgentNotification::PublicationAdded(_)
                | AgentNotification::PublicationRemoved { .. }
                | AgentNotification::VideoBindingsChanged
                | AgentNotification::AudioBindingsChanged => state_changed = true,
                AgentNotification::Topic(delivery) => self.deliver_topic(&delivery),
                AgentNotification::Error(error) => match error {
                    AgentError::Protocol(message) => self.error("protocol", message),
                    AgentError::Terminal(message) => self.error("terminal", message),
                    AgentError::Topic(message) => self.error("topic", message),
                },
            }
        }
        if state_changed {
            self.publish_state();
        }
    }

    fn observe_transport_lifecycle(&mut self, effect: &AgentEffect) -> bool {
        match effect {
            AgentEffect::Rtc(agent_core::RtcEffect::CreateTransport { generation, .. }) => {
                self.remote.begin_generation(*generation)
            }
            AgentEffect::Rtc(agent_core::RtcEffect::CloseTransport { generation }) => {
                self.remote.end_generation(*generation)
            }
            _ => false,
        }
    }

    fn reconcile_remote(&mut self) -> bool {
        let result = self.remote.reconcile(self.core.snapshot());
        if result.removed_video {
            let available = self.remote.available_video_ids();
            self.video_intents
                .retain(|track_id, _| available.contains(track_id));
        }
        result.changed
    }

    fn deliver_topic(&self, delivery: &TopicDelivery) {
        match delivery {
            TopicDelivery::Latest {
                registration,
                payload,
            } => {
                let Some(sink) = self.topic_sinks.get(&registration.topic) else {
                    return;
                };
                if sink.kind != TopicKind::Latest {
                    return;
                }
                let message = js_sys::Object::new();
                let _ = set_js(
                    &message,
                    "data",
                    js_sys::Uint8Array::from(payload.as_slice()).into(),
                );
                __pulsebeam_emit_topic_message(&sink.handle, &message.into());
            }
            TopicDelivery::Ordered {
                registration,
                delivery,
            } => {
                let Some(sink) = self.topic_sinks.get(&registration.topic) else {
                    return;
                };
                if sink.kind != TopicKind::Ordered {
                    return;
                }
                match delivery {
                    OrderedDelivery::Message {
                        publisher_id,
                        stream_id,
                        sequence,
                        payload,
                    } => {
                        let message = js_sys::Object::new();
                        let _ = set_js(
                            &message,
                            "data",
                            js_sys::Uint8Array::from(payload.as_slice()).into(),
                        );
                        let _ = set_js(&message, "publisherId", JsValue::from_str(publisher_id));
                        let _ = set_js(&message, "streamId", JsValue::from_str(&stream_id.to_string()));
                        let _ = set_js(&message, "sequence", JsValue::from_str(&sequence.to_string()));
                        __pulsebeam_emit_topic_message(&sink.handle, &message.into());
                    }
                    OrderedDelivery::Resync {
                        publisher_id,
                        stream_id,
                    } => {
                        let event = js_sys::Object::new();
                        let _ = set_js(&event, "publisherId", JsValue::from_str(publisher_id));
                        let _ = set_js(&event, "streamId", JsValue::from_str(&stream_id.to_string()));
                        __pulsebeam_emit_topic_resync(&sink.handle, &event.into());
                    }
                }
            }
        }
    }

    fn publish_state(&mut self) {
        let snapshot = self
            .remote
            .agent_snapshot(self.core.snapshot())
            .unwrap_or_else(|error| {
                self.error("bridge", js_message(&error));
                empty_agent_snapshot()
            });
        if self.snapshots.send(snapshot).is_ok() {
            notify(&self.listeners);
        }
    }

    fn error(&self, kind: &str, message: impl AsRef<str>) {
        let error = js_sys::Object::new();
        let _ = set_js(&error, "kind", JsValue::from_str(kind));
        let _ = set_js(&error, "message", JsValue::from_str(message.as_ref()));
        emit(&self.errors, &error.into());
    }
}

// -----------------------------------------------------------------------------
// Remote publication -> stable JS handle projection
// -----------------------------------------------------------------------------

#[derive(Clone)]
struct RemoteMedia {
    publication: Publication,
    mid: Option<String>,
    paused: bool,
    track: Option<web_sys::MediaStreamTrack>,
}

struct VideoHandleEntry {
    participant_id: String,
    handle: JsValue,
}

struct AudioHandleEntry {
    participant_id: String,
    handle: JsValue,
}

struct ReconcileResult {
    changed: bool,
    removed_video: bool,
}

struct RemoteRegistry {
    owner: JsValue,
    commands: mpsc::Sender<DriverCommand>,
    generation: Option<agent_core::Generation>,
    items: BTreeMap<String, RemoteMedia>,
    tracks: BTreeMap<String, web_sys::MediaStreamTrack>,
    video_handles: BTreeMap<String, VideoHandleEntry>,
    audio_handles: BTreeMap<String, AudioHandleEntry>,
}

impl RemoteRegistry {
    fn new(owner: JsValue, commands: mpsc::Sender<DriverCommand>) -> Self {
        Self {
            owner,
            commands,
            generation: None,
            items: BTreeMap::new(),
            tracks: BTreeMap::new(),
            video_handles: BTreeMap::new(),
            audio_handles: BTreeMap::new(),
        }
    }

    fn begin_generation(&mut self, generation: agent_core::Generation) -> bool {
        let changed = self.generation != Some(generation)
            || !self.items.is_empty()
            || !self.tracks.is_empty();
        self.mark_all_unavailable();
        self.generation = Some(generation);
        self.items.clear();
        self.tracks.clear();
        changed
    }

    fn end_generation(&mut self, generation: agent_core::Generation) -> bool {
        if self.generation != Some(generation) {
            return false;
        }
        let changed = self.generation.is_some() || !self.items.is_empty() || !self.tracks.is_empty();
        self.mark_all_unavailable();
        self.generation = None;
        self.items.clear();
        self.tracks.clear();
        changed
    }

    fn track(
        &mut self,
        generation: agent_core::Generation,
        mid: String,
        track: web_sys::MediaStreamTrack,
    ) -> bool {
        if self.generation != Some(generation) {
            return false;
        }
        let changed = self.tracks.get(&mid).is_none_or(|current| current != &track);
        self.tracks.insert(mid, track);
        changed
    }

    fn reconcile(&mut self, snapshot: &AgentSnapshot) -> ReconcileResult {
        let previous_ids: BTreeSet<_> = self.items.keys().cloned().collect();
        self.items.retain(|track_id, _| {
            snapshot
                .publications
                .iter()
                .any(|publication| publication.track_id == *track_id)
        });
        let mut changed = self.items.len() != previous_ids.len();
        let mut removed_video = false;

        for removed in previous_ids.iter().filter(|id| !self.items.contains_key(*id)) {
            if let Some(handle) = self.video_handles.get(removed) {
                Self::update_video_handle(handle, None);
                removed_video = true;
            }
            if let Some(handle) = self.audio_handles.get(removed) {
                Self::update_audio_handle(handle, None, None);
            }
        }

        for publication in &snapshot.publications {
            let binding = match publication.kind {
                MediaKind::Video => snapshot
                    .video_bindings
                    .iter()
                    .find(|binding| binding.track_id == publication.track_id)
                    .map(|binding| (binding.mid.clone(), binding.paused)),
                MediaKind::Audio => snapshot
                    .audio_bindings
                    .iter()
                    .find(|binding| binding.track_id == publication.track_id)
                    .map(|binding| (binding.mid.clone(), false)),
            };
            let mid = binding.as_ref().map(|(mid, _)| mid.clone());
            let paused = binding.as_ref().is_some_and(|(_, paused)| *paused);
            let track = mid.as_ref().and_then(|mid| self.tracks.get(mid)).cloned();
            let next = RemoteMedia {
                publication: publication.clone(),
                mid,
                paused,
                track,
            };
            let item_changed = self
                .items
                .get(&publication.track_id)
                .is_none_or(|current| !same_remote_media(current, &next));
            if item_changed {
                self.items.insert(publication.track_id.clone(), next.clone());
                changed = true;
            }

            match publication.kind {
                MediaKind::Video => {
                    if item_changed || !self.video_handles.contains_key(&publication.track_id) {
                        let handle = self.ensure_video_handle(publication, &next);
                        Self::update_video_handle(handle, Some(&next));
                    }
                }
                MediaKind::Audio => {
                    let level = snapshot
                        .audio_bindings
                        .iter()
                        .find(|binding| binding.track_id == publication.track_id)
                        .map(|binding| binding.level_dbov);
                    let handle = self.ensure_audio_handle(publication, &next, level);
                    Self::update_audio_handle(handle, Some(&next), level);
                }
            }
        }

        ReconcileResult {
            changed,
            removed_video,
        }
    }

    fn ensure_video_handle(
        &mut self,
        publication: &Publication,
        media: &RemoteMedia,
    ) -> &VideoHandleEntry {
        let replace = self
            .video_handles
            .get(&publication.track_id)
            .is_some_and(|entry| entry.participant_id != publication.participant_id);
        if replace {
            if let Some(old) = self.video_handles.remove(&publication.track_id) {
                Self::update_video_handle(&old, None);
            }
        }
        let owner = self.owner.clone();
        let commands = self.commands.clone();
        self.video_handles
            .entry(publication.track_id.clone())
            .or_insert_with(|| {
                let initial = video_snapshot(Some(media));
                let set_state =
                    remote_video_set_state_callback(commands, publication.track_id.clone());
                let handle = __pulsebeam_make_remote_video(
                    &owner,
                    &publication.track_id,
                    &publication.participant_id,
                    &initial,
                    &set_state,
                );
                VideoHandleEntry {
                    participant_id: publication.participant_id.clone(),
                    handle,
                }
            })
    }

    fn ensure_audio_handle(
        &mut self,
        publication: &Publication,
        media: &RemoteMedia,
        level: Option<i32>,
    ) -> &AudioHandleEntry {
        let replace = self
            .audio_handles
            .get(&publication.track_id)
            .is_some_and(|entry| entry.participant_id != publication.participant_id);
        if replace {
            if let Some(old) = self.audio_handles.remove(&publication.track_id) {
                Self::update_audio_handle(&old, None, None);
            }
        }
        let owner = self.owner.clone();
        self.audio_handles
            .entry(publication.track_id.clone())
            .or_insert_with(|| {
                let initial = audio_snapshot(Some(media), level);
                let handle = __pulsebeam_make_remote_audio(
                    &owner,
                    &publication.track_id,
                    &publication.participant_id,
                    &initial,
                );
                AudioHandleEntry {
                    participant_id: publication.participant_id.clone(),
                    handle,
                }
            })
    }

    fn update_video_handle(entry: &VideoHandleEntry, media: Option<&RemoteMedia>) {
        let snapshot = video_snapshot(media);
        __pulsebeam_update_store(&entry.handle, &snapshot);
    }

    fn update_audio_handle(
        entry: &AudioHandleEntry,
        media: Option<&RemoteMedia>,
        level: Option<i32>,
    ) {
        let snapshot = audio_snapshot(media, level);
        __pulsebeam_update_store(&entry.handle, &snapshot);
    }

    fn mark_all_unavailable(&self) {
        for handle in self.video_handles.values() {
            Self::update_video_handle(handle, None);
        }
        for handle in self.audio_handles.values() {
            Self::update_audio_handle(handle, None, None);
        }
    }

    fn is_available_video(&self, track_id: &str) -> bool {
        self.items
            .get(track_id)
            .is_some_and(|media| media.publication.kind == MediaKind::Video)
    }

    fn available_video_ids(&self) -> BTreeSet<String> {
        self.items
            .iter()
            .filter(|(_, media)| media.publication.kind == MediaKind::Video)
            .map(|(track_id, _)| track_id.clone())
            .collect()
    }

    fn agent_snapshot(&self, snapshot: &AgentSnapshot) -> Result<JsValue, JsValue> {
        let object = js_sys::Object::new();
        set_js(
            &object,
            "connection",
            JsValue::from_str(connection_phase(&snapshot.connection)),
        )?;
        set_js(
            &object,
            "participantId",
            snapshot
                .participant_id
                .as_ref()
                .map(|value| JsValue::from_str(value))
                .unwrap_or(JsValue::NULL),
        )?;

        let video = js_sys::Array::new();
        let audio = js_sys::Array::new();
        for publication in &snapshot.publications {
            match publication.kind {
                MediaKind::Video => {
                    if let Some(handle) = self.video_handles.get(&publication.track_id) {
                        video.push(&handle.handle);
                    }
                }
                MediaKind::Audio => {
                    if let Some(handle) = self.audio_handles.get(&publication.track_id) {
                        audio.push(&handle.handle);
                    }
                }
            }
        }
        set_js(&object, "videoTracks", video.into())?;
        set_js(&object, "audioTracks", audio.into())?;
        Ok(object.into())
    }
}

fn same_remote_media(left: &RemoteMedia, right: &RemoteMedia) -> bool {
    left.publication == right.publication
        && left.mid == right.mid
        && left.paused == right.paused
        && left.track == right.track
}

fn empty_agent_snapshot() -> JsValue {
    let object = js_sys::Object::new();
    let _ = set_js(&object, "connection", JsValue::from_str("disconnected"));
    let _ = set_js(&object, "participantId", JsValue::NULL);
    let _ = set_js(&object, "videoTracks", js_sys::Array::new().into());
    let _ = set_js(&object, "audioTracks", js_sys::Array::new().into());
    object.into()
}

fn video_snapshot(media: Option<&RemoteMedia>) -> JsValue {
    let object = js_sys::Object::new();
    let available = media.is_some();
    let _ = set_js(&object, "available", JsValue::from_bool(available));
    let _ = set_js(
        &object,
        "mid",
        media
            .and_then(|media| media.mid.as_ref())
            .map(|mid| JsValue::from_str(mid))
            .unwrap_or(JsValue::NULL),
    );
    let _ = set_js(
        &object,
        "paused",
        JsValue::from_bool(media.map_or(true, |media| media.paused)),
    );
    let _ = set_js(
        &object,
        "mediaStreamTrack",
        media
            .and_then(|media| media.track.clone())
            .map(JsValue::from)
            .unwrap_or(JsValue::NULL),
    );
    object.into()
}

fn audio_snapshot(media: Option<&RemoteMedia>, level: Option<i32>) -> JsValue {
    let object = js_sys::Object::new();
    let _ = set_js(&object, "available", JsValue::from_bool(media.is_some()));
    let _ = set_js(
        &object,
        "mid",
        media
            .and_then(|media| media.mid.as_ref())
            .map(|mid| JsValue::from_str(mid))
            .unwrap_or(JsValue::NULL),
    );
    let _ = set_js(
        &object,
        "levelDbov",
        level
            .map(|value| JsValue::from_f64(value as f64))
            .unwrap_or(JsValue::NULL),
    );
    let _ = set_js(
        &object,
        "mediaStreamTrack",
        media
            .and_then(|media| media.track.clone())
            .map(JsValue::from)
            .unwrap_or(JsValue::NULL),
    );
    object.into()
}

// -----------------------------------------------------------------------------
// Browser effect runtime
// -----------------------------------------------------------------------------
struct BrowserRuntime {
    transport: Option<Transport>,
    timers: BTreeMap<agent_core::TimerId, gloo_timers::callback::Timeout>,
    requests: BTreeMap<agent_core::RequestId, web_sys::AbortController>,
    local_tracks: Vec<LocalSlotTracks>,
    local_slots: Vec<agent_core::LocalSlotIntent>,
}

struct Transport {
    generation: agent_core::Generation,
    peer: web_sys::RtcPeerConnection,
    channels: BTreeMap<agent_core::DataChannelId, Channel>,
    _connection_callback: wasm_bindgen::closure::Closure<dyn FnMut(web_sys::Event)>,
    _track_callback: wasm_bindgen::closure::Closure<dyn FnMut(web_sys::RtcTrackEvent)>,
    topology: WebTopology,
}

#[derive(Clone)]
struct WebTopology {
    upstream: Vec<(
        String,
        web_sys::RtcRtpTransceiver,
        web_sys::RtcRtpTransceiver,
    )>,
    video: Vec<web_sys::RtcRtpTransceiver>,
    audio: Vec<web_sys::RtcRtpTransceiver>,
}

struct Channel {
    channel: web_sys::RtcDataChannel,
    label: String,
    _open_callback: wasm_bindgen::closure::Closure<dyn FnMut(web_sys::Event)>,
    _close_callback: wasm_bindgen::closure::Closure<dyn FnMut(web_sys::Event)>,
    _message_callback: wasm_bindgen::closure::Closure<dyn FnMut(web_sys::MessageEvent)>,
    _buffered_amount_low_callback: wasm_bindgen::closure::Closure<dyn FnMut(web_sys::Event)>,
}

impl BrowserRuntime {
    fn new() -> Self {
        Self {
            transport: None,
            timers: BTreeMap::new(),
            requests: BTreeMap::new(),
            local_tracks: Vec::new(),
            local_slots: Vec::new(),
        }
    }

    fn execute(&mut self, effect: AgentEffect, commands: mpsc::Sender<DriverCommand>) {
        match effect {
            AgentEffect::Rtc(effect) => self.rtc(effect, commands),
            AgentEffect::DataChannel(effect) => self.channel(effect, commands),
            AgentEffect::Timer(effect) => self.timer(effect, commands),
            AgentEffect::Http(effect) => self.http(effect, commands),
        }
    }

    fn set_local_tracks(&mut self, tracks: Vec<LocalSlotTracks>) {
        self.local_tracks = tracks;
        self.apply_media();
    }

    fn set_local_slots(&mut self, slots: Vec<agent_core::LocalSlotIntent>) {
        self.local_slots = slots;
        self.apply_media();
    }

    fn apply_media(&self) {
        let Some(transport) = &self.transport else {
            return;
        };

        for (name, audio, video) in &transport.topology.upstream {
            let intent = self.local_slots.iter().find(|slot| slot.slot == *name);
            let supplied = self.local_tracks.iter().find(|slot| slot.slot == *name);

            let audio_track = intent
                .filter(|slot| slot.audio.attached && !slot.audio.muted)
                .and_then(|_| supplied.and_then(|slot| slot.audio.as_ref()));
            let video_track = intent
                .filter(|slot| slot.video.attached && !slot.video.muted)
                .and_then(|_| supplied.and_then(|slot| slot.video.as_ref()));

            let _ = audio.sender().replace_track(audio_track);
            let _ = video.sender().replace_track(video_track);
        }
    }

    fn can_send(&self, registration: &TopicRegistration, payload_len: usize) -> bool {
        let Some(transport) = &self.transport else {
            return false;
        };

        let label = agent_core::topic_label(registration);
        let Some(channel) = transport
            .channels
            .values()
            .find(|channel| channel.label == label)
        else {
            return false;
        };

        if channel.channel.ready_state() != web_sys::RtcDataChannelState::Open {
            return false;
        }

        let payload_len = u64::try_from(payload_len).unwrap_or(u64::MAX);
        u64::from(channel.channel.buffered_amount()).saturating_add(payload_len)
            <= TOPIC_BUFFERED_AMOUNT_LIMIT
    }

    fn timer_finished(&mut self, id: agent_core::TimerId) {
        let _ = self.timers.remove(&id);
    }

    fn request_finished(&mut self, id: agent_core::RequestId) {
        let _ = self.requests.remove(&id);
    }

    fn abort_requests(&mut self) {
        for controller in self.requests.values() {
            controller.abort();
        }
        self.requests.clear();
    }

    fn close_all(&mut self) {
        self.abort_requests();
        self.timers.clear();

        if let Some(transport) = self.transport.take() {
            transport.peer.close();
        }
    }

    fn finish_graceful_close(&mut self) {
        // Explicit WebAgent.close() has already allowed outstanding HTTP to finish.
        // Do not abort a DELETE request here. Timers and browser transport state can
        // simply be discarded once the driver is terminal.
        self.requests.clear();
        self.timers.clear();

        if let Some(transport) = self.transport.take() {
            transport.peer.close();
        }
    }

    fn graceful_close_ready(&self) -> bool {
        self.requests.is_empty()
    }

    fn rtc(&mut self, effect: agent_core::RtcEffect, commands: mpsc::Sender<DriverCommand>) {
        match effect {
            agent_core::RtcEffect::CreateTransport {
                generation,
                topology,
                ..
            } => self.create_transport(generation, topology, commands),

            agent_core::RtcEffect::ApplyAnswer { generation, answer } => {
                let Some(transport) = self
                    .transport
                    .as_ref()
                    .filter(|transport| transport.generation == generation)
                else {
                    return;
                };

                let peer = transport.peer.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let description =
                        web_sys::RtcSessionDescriptionInit::new(web_sys::RtcSdpType::Answer);
                    description.set_sdp(&answer);

                    let event = if wasm_bindgen_futures::JsFuture::from(
                        peer.set_remote_description(&description),
                    )
                    .await
                    .is_ok()
                    {
                        AgentEvent::Rtc(agent_core::RtcEvent::AnswerApplied { generation })
                    } else {
                        AgentEvent::Rtc(agent_core::RtcEvent::Disconnected { generation })
                    };

                    let _ = commands.send(DriverCommand::CoreEvent(event));
                });
            }

            agent_core::RtcEffect::CloseTransport { generation } => {
                if self
                    .transport
                    .as_ref()
                    .is_some_and(|transport| transport.generation == generation)
                {
                    if let Some(transport) = self.transport.take() {
                        transport.peer.close();
                    }
                }
            }

            agent_core::RtcEffect::ReconcileLocalSlots { slots, .. } => {
                self.set_local_slots(slots);
            }
        }
    }

    fn create_transport(
        &mut self,
        generation: agent_core::Generation,
        topology: agent_core::Topology,
        commands: mpsc::Sender<DriverCommand>,
    ) {
        self.abort_requests();
        if let Some(previous) = self.transport.take() {
            previous.peer.close();
        }

        let Ok(peer) = web_sys::RtcPeerConnection::new() else {
            let _ = commands.send(DriverCommand::CoreEvent(AgentEvent::Rtc(
                agent_core::RtcEvent::Disconnected { generation },
            )));
            return;
        };

        let callback_peer = peer.clone();
        let connection_commands = commands.clone();
        let connection_callback = wasm_bindgen::closure::Closure::wrap(Box::new(move |_| {
            if matches!(
                callback_peer.connection_state(),
                web_sys::RtcPeerConnectionState::Disconnected
                    | web_sys::RtcPeerConnectionState::Failed
                    | web_sys::RtcPeerConnectionState::Closed
            ) {
                let _ = connection_commands.send(DriverCommand::CoreEvent(AgentEvent::Rtc(
                    agent_core::RtcEvent::Disconnected { generation },
                )));
            }
        }) as Box<dyn FnMut(web_sys::Event)>);
        peer.set_onconnectionstatechange(Some(connection_callback.as_ref().unchecked_ref()));

        let track_commands = commands.clone();
        let track_callback = wasm_bindgen::closure::Closure::wrap(Box::new(
            move |event: web_sys::RtcTrackEvent| {
                let Some(mid) = event.transceiver().mid() else {
                    return;
                };
                let _ = track_commands.send(DriverCommand::RemoteTrack {
                    generation,
                    mid,
                    track: event.track(),
                });
            },
        ) as Box<dyn FnMut(web_sys::RtcTrackEvent)>);
        peer.set_ontrack(Some(track_callback.as_ref().unchecked_ref()));

        let web_topology = WebTopology::create(&peer, &topology);
        let offer_peer = peer.clone();
        let offer_topology = web_topology.clone();
        let offer_commands = commands.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let Ok(offer) = wasm_bindgen_futures::JsFuture::from(offer_peer.create_offer()).await
            else {
                let _ = offer_commands.send(DriverCommand::CoreEvent(AgentEvent::Rtc(
                    agent_core::RtcEvent::Disconnected { generation },
                )));
                return;
            };

            let offer: web_sys::RtcSessionDescriptionInit = offer.unchecked_into();
            if wasm_bindgen_futures::JsFuture::from(offer_peer.set_local_description(&offer))
                .await
                .is_err()
            {
                let _ = offer_commands.send(DriverCommand::CoreEvent(AgentEvent::Rtc(
                    agent_core::RtcEvent::Disconnected { generation },
                )));
                return;
            }

            let Some(sdp) = js_sys::Reflect::get(&offer, &JsValue::from_str("sdp"))
                .ok()
                .and_then(|value| value.as_string())
            else {
                let _ = offer_commands.send(DriverCommand::CoreEvent(AgentEvent::Rtc(
                    agent_core::RtcEvent::Disconnected { generation },
                )));
                return;
            };

            let Some(topology) = offer_topology.negotiated() else {
                let _ = offer_commands.send(DriverCommand::CoreEvent(AgentEvent::Rtc(
                    agent_core::RtcEvent::Disconnected { generation },
                )));
                return;
            };

            let _ = offer_commands.send(DriverCommand::CoreEvent(AgentEvent::Rtc(
                agent_core::RtcEvent::OfferCreated {
                    generation,
                    offer: sdp,
                    topology,
                },
            )));
        });

        self.transport = Some(Transport {
            generation,
            peer,
            channels: BTreeMap::new(),
            _connection_callback: connection_callback,
            _track_callback: track_callback,
            topology: web_topology,
        });

        self.apply_media();
    }

    fn channel(
        &mut self,
        effect: agent_core::DataChannelEffect,
        commands: mpsc::Sender<DriverCommand>,
    ) {
        match effect {
            agent_core::DataChannelEffect::Open {
                generation,
                id,
                config,
            } => {
                let Some(transport) = self
                    .transport
                    .as_mut()
                    .filter(|transport| transport.generation == generation)
                else {
                    return;
                };

                let options = web_sys::RtcDataChannelInit::new();
                options.set_ordered(config.ordered);
                options.set_protocol(&config.protocol);
                if let agent_core::DataChannelReliability::MaxRetransmits(value) =
                    config.reliability
                {
                    options.set_max_retransmits(value);
                }

                let label = config.label.clone();
                let channel = transport
                    .peer
                    .create_data_channel_with_data_channel_dict(&config.label, &options);

                let open_commands = commands.clone();
                let open_callback = wasm_bindgen::closure::Closure::wrap(Box::new(move |_| {
                    let _ = open_commands.send(DriverCommand::CoreEvent(AgentEvent::DataChannel(
                        agent_core::DataChannelEvent::Opened { generation, id },
                    )));
                }) as Box<dyn FnMut(web_sys::Event)>);
                channel.set_onopen(Some(open_callback.as_ref().unchecked_ref()));

                let close_commands = commands.clone();
                let close_callback = wasm_bindgen::closure::Closure::wrap(Box::new(move |_| {
                    let _ = close_commands.send(DriverCommand::CoreEvent(
                        AgentEvent::DataChannel(agent_core::DataChannelEvent::Closed {
                            generation,
                            id,
                        }),
                    ));
                }) as Box<dyn FnMut(web_sys::Event)>);
                channel.set_onclose(Some(close_callback.as_ref().unchecked_ref()));

                let message_commands = commands.clone();
                let message_callback = wasm_bindgen::closure::Closure::wrap(Box::new(
                    move |event: web_sys::MessageEvent| {
                        let data = event.data();
                        if !data.is_instance_of::<js_sys::ArrayBuffer>() {
                            return;
                        }

                        let payload = js_sys::Uint8Array::new(&data).to_vec();
                        let _ = message_commands.send(DriverCommand::CoreEvent(
                            AgentEvent::DataChannel(agent_core::DataChannelEvent::Message {
                                generation,
                                id,
                                payload,
                            }),
                        ));
                    },
                ) as Box<dyn FnMut(web_sys::MessageEvent)>);
                channel.set_onmessage(Some(message_callback.as_ref().unchecked_ref()));

                channel.set_buffered_amount_low_threshold(
                    u32::try_from(TOPIC_BUFFERED_AMOUNT_LIMIT / 2).unwrap_or(u32::MAX),
                );
                let writable_commands = commands.clone();
                let buffered_amount_low_callback =
                    wasm_bindgen::closure::Closure::wrap(Box::new(move |_| {
                        let _ = writable_commands.send(DriverCommand::TopicWritable);
                    }) as Box<dyn FnMut(web_sys::Event)>);
                channel.set_onbufferedamountlow(Some(
                    buffered_amount_low_callback.as_ref().unchecked_ref(),
                ));

                transport.channels.insert(
                    id,
                    Channel {
                        channel,
                        label,
                        _open_callback: open_callback,
                        _close_callback: close_callback,
                        _message_callback: message_callback,
                        _buffered_amount_low_callback: buffered_amount_low_callback,
                    },
                );
            }

            agent_core::DataChannelEffect::Close { generation, id } => {
                if let Some(transport) = self
                    .transport
                    .as_mut()
                    .filter(|transport| transport.generation == generation)
                    && let Some(channel) = transport.channels.remove(&id)
                {
                    channel.channel.close();
                }
            }

            agent_core::DataChannelEffect::Send {
                generation,
                id,
                payload,
            } => {
                let Some(channel) = self
                    .transport
                    .as_ref()
                    .filter(|transport| transport.generation == generation)
                    .and_then(|transport| transport.channels.get(&id))
                else {
                    return;
                };

                if channel.channel.send_with_u8_array(&payload).is_err() {
                    let _ = commands.send(DriverCommand::CoreEvent(AgentEvent::DataChannel(
                        agent_core::DataChannelEvent::WriteFailed { generation, id },
                    )));
                }
            }
        }
    }

    fn timer(
        &mut self,
        effect: agent_core::TimerEffect,
        commands: mpsc::Sender<DriverCommand>,
    ) {
        match effect {
            agent_core::TimerEffect::Schedule { id, after } => {
                let millis = u32::try_from(after.as_millis()).unwrap_or(u32::MAX);
                let timeout = gloo_timers::callback::Timeout::new(millis, move || {
                    let _ = commands.send(DriverCommand::TimerFired { id });
                });
                self.timers.insert(id, timeout);
            }
            agent_core::TimerEffect::Cancel { id } => {
                let _ = self.timers.remove(&id);
            }
        }
    }

    fn http(
        &mut self,
        effect: agent_core::HttpEffect,
        commands: mpsc::Sender<DriverCommand>,
    ) {
        let agent_core::HttpEffect::Request { id, request } = effect;

        let controller = web_sys::AbortController::new().ok();
        let signal = controller.as_ref().map(web_sys::AbortController::signal);
        let mut builder =
            gloo_net::http::RequestBuilder::new(&request.uri).abort_signal(signal.as_ref());
        builder = builder.method(match request.method {
            agent_core::http::HttpMethod::Get => gloo_net::http::Method::GET,
            agent_core::http::HttpMethod::Post => gloo_net::http::Method::POST,
            agent_core::http::HttpMethod::Put => gloo_net::http::Method::PUT,
            agent_core::http::HttpMethod::Patch => gloo_net::http::Method::PATCH,
            agent_core::http::HttpMethod::Delete => gloo_net::http::Method::DELETE,
        });
        for header in &request.headers {
            builder = builder.header(&header.name, &header.value);
        }

        let Ok(request) = builder.body(JsValue::from(js_sys::Uint8Array::from(
            request.body.as_slice(),
        ))) else {
            let _ = commands.send(DriverCommand::CoreEvent(AgentEvent::Http(
                agent_core::HttpEvent::Failed { id },
            )));
            return;
        };

        if let Some(controller) = controller {
            self.requests.insert(id, controller);
        }

        wasm_bindgen_futures::spawn_local(async move {
            let event = match request.send().await {
                Ok(response) => {
                    let status = response.status();
                    let headers = response
                        .headers()
                        .entries()
                        .map(|(name, value)| agent_core::http::HttpHeader { name, value })
                        .collect();

                    match response.binary().await {
                        Ok(body) => AgentEvent::Http(agent_core::HttpEvent::Response {
                            id,
                            response: agent_core::http::HttpResponse {
                                status,
                                headers,
                                body,
                            },
                        }),
                        Err(_) => AgentEvent::Http(agent_core::HttpEvent::Failed { id }),
                    }
                }
                Err(_) => AgentEvent::Http(agent_core::HttpEvent::Failed { id }),
            };

            let _ = commands.send(DriverCommand::HttpFinished { id, event });
        });
    }
}


impl WebTopology {
    fn create(peer: &web_sys::RtcPeerConnection, topology: &agent_core::Topology) -> Self {
        let upstream = topology
            .upstream_slots()
            .iter()
            .map(|slot| {
                let audio =
                    transceiver(peer, "audio", web_sys::RtcRtpTransceiverDirection::Sendrecv);
                let video =
                    transceiver(peer, "video", web_sys::RtcRtpTransceiverDirection::Sendrecv);
                (String::from(slot.name()), audio, video)
            })
            .collect();

        let video = (0..topology.video_receive_slots())
            .map(|_| transceiver(peer, "video", web_sys::RtcRtpTransceiverDirection::Recvonly))
            .collect();
        let audio = (0..topology.audio_receive_slots())
            .map(|_| transceiver(peer, "audio", web_sys::RtcRtpTransceiverDirection::Recvonly))
            .collect();

        Self {
            upstream,
            video,
            audio,
        }
    }

    fn negotiated(&self) -> Option<agent_core::NegotiatedTopology> {
        Some(agent_core::NegotiatedTopology {
            upstream_slots: self
                .upstream
                .iter()
                .map(|(slot, audio, video)| {
                    Some(agent_core::NegotiatedUpstreamSlot {
                        slot: slot.clone(),
                        audio_mid: audio.mid()?,
                        video_mid: video.mid()?,
                    })
                })
                .collect::<Option<Vec<_>>>()?,
            video_receive_mids: self
                .video
                .iter()
                .map(web_sys::RtcRtpTransceiver::mid)
                .collect::<Option<Vec<_>>>()?,
            audio_receive_mids: self
                .audio
                .iter()
                .map(web_sys::RtcRtpTransceiver::mid)
                .collect::<Option<Vec<_>>>()?,
        })
    }
}

fn transceiver(
    peer: &web_sys::RtcPeerConnection,
    kind: &str,
    direction: web_sys::RtcRtpTransceiverDirection,
) -> web_sys::RtcRtpTransceiver {
    let init = web_sys::RtcRtpTransceiverInit::new();
    init.set_direction(direction);
    peer.add_transceiver_with_str_and_init(kind, &init)
}

