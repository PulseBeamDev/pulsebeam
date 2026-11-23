use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::{Rc, Weak},
};

use agent_core::{
    AgentConfig, ChannelId, DataChannelBinding, DataChannelEffect, DataChannelEvent,
    DataChannelReliability, DataChannelSpec, Effect, Failure, Generation, HostEvent, HttpEffect,
    HttpEvent, HttpHeader, HttpMethod, HttpResponse, MediaKind, MediaSlot, OfferResources,
    OperationId, RetryPolicy, RtcEffect, RtcEvent, TimerEffect, TimerEvent,
};
use js_sys::{Array, Function, Object, Reflect, Uint8Array};
use wasm_bindgen::{JsCast, JsValue, closure::Closure};
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{
    AbortController, Event, Headers, MediaStreamTrack, MessageEvent, Request, RequestInit,
    Response, RtcBundlePolicy, RtcConfiguration, RtcDataChannel, RtcDataChannelInit,
    RtcDataChannelState, RtcDataChannelType, RtcPeerConnection, RtcPeerConnectionState,
    RtcRtpSender, RtcRtpTransceiver, RtcRtpTransceiverDirection, RtcRtpTransceiverInit, RtcSdpType,
    RtcSessionDescriptionInit, RtcTrackEvent,
};

use crate::engine::{ActorHandle, Host, PublicCommand, Turn, TurnErrorSource, spawn_actor};
use crate::local_operation::LocalOperationGate;
use crate::logger::{BrowserLogger, LogLevel};
use crate::sender_config::{
    EncodingConfig, SenderConfig, media_kind_name, normalize_sender_config,
};

pub(crate) mod catalog;
pub(crate) mod media_observation;
mod preparation;
pub(crate) mod publication;
mod resource_state;
use publication::BrowserPipe;
use resource_state::ResourceObserver;

const SIGNALING_LABEL: &str = "v1/sys/signaling";
const LOG_TARGET: &str = "pulsebeam_agent_web::browser";

fn same_js_object<T: AsRef<JsValue>>(left: &T, right: &T) -> bool {
    let left: &JsValue = left.as_ref();
    let right: &JsValue = right.as_ref();
    left == right
}

pub(crate) fn input_is_live(track: &MediaStreamTrack) -> bool {
    Reflect::get(track.as_ref(), &JsValue::from_str("readyState"))
        .ok()
        .and_then(|value| value.as_string())
        .as_deref()
        == Some("live")
}

struct LocalOperationError {
    class: &'static str,
    message: String,
}

impl LocalOperationError {
    fn validation(message: impl Into<String>) -> Self {
        Self {
            class: "validation",
            message: message.into(),
        }
    }

    fn runtime(message: impl Into<String>) -> Self {
        Self {
            class: "runtime",
            message: message.into(),
        }
    }

    fn error_info(&self) -> agent_core::ffi::ErrorInfo {
        let (code, message) = match self.class {
            "validation" => (
                agent_core::ffi::ErrorCode::InvalidArgument,
                "Invalid local operation argument",
            ),
            _ => (
                agent_core::ffi::ErrorCode::Internal,
                "Local media operation failed",
            ),
        };
        agent_core::ffi::ErrorInfo::new(code, message)
    }

    fn into_js(self) -> JsValue {
        let error = js_sys::Error::new(&self.message);
        let _ = Reflect::set(
            error.as_ref(),
            &JsValue::from_str("pulsebeamInfo"),
            &error_info_value(&self.error_info()),
        );
        let _ = Reflect::set(
            error.as_ref(),
            &JsValue::from_str("pulsebeamClass"),
            &JsValue::from_str(self.class),
        );
        error.into()
    }
}

#[derive(Clone, Copy)]
struct PublicationInputRequest {
    owner: u64,
    muted: Option<bool>,
    intent: agent_core::PublicationInputIntent,
}

#[derive(Clone)]
struct LocalTrackState {
    track: MediaStreamTrack,
    config: SenderConfig,
    source_id: String,
    after_generation: Option<Generation>,
    input_operation: Option<agent_core::PublicationInputOperation>,
}

#[derive(Clone)]
struct PublicationSlot {
    slot: String,
    kind: MediaKind,
    operations: Rc<LocalSlotOperations>,
    after_generation: Option<Generation>,
}

struct LocalSlotOperations {
    slot: String,
    inner: Weak<RuntimeInner>,
    gate: LocalOperationGate,
}

impl Drop for LocalSlotOperations {
    fn drop(&mut self) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        let mut gates = inner.local_operation_gates.borrow_mut();
        if gates
            .get(&self.slot)
            .is_some_and(|entry| std::ptr::eq(entry.as_ptr(), self))
        {
            gates.remove(&self.slot);
        }
    }
}

fn prepared_data_channel(
    connection: &RtcPeerConnection,
    channel: &RtcDataChannel,
) -> Result<agent_core::PreparedDataChannel, String> {
    let boolean_property = |name: &str| {
        Reflect::get(channel.as_ref(), &JsValue::from_str(name))
            .ok()
            .and_then(|value| value.as_bool())
            .ok_or_else(|| "browser channel facts unavailable".to_owned())
    };
    let reliability = match (channel.max_retransmits(), channel.max_packet_life_time()) {
        (None, None) => DataChannelReliability::Reliable,
        (Some(count), None) => DataChannelReliability::MaxRetransmits(count),
        (None, Some(time)) => DataChannelReliability::MaxPacketLifetime(time),
        _ => return Err("conflicting browser retransmit limits".to_owned()),
    };
    // Pinned web-sys omits RTCSctpTransport and its nullable maxChannels getter.
    let sctp = Reflect::get(connection.as_ref(), &JsValue::from_str("sctp"))
        .map_err(|_| "browser SCTP transport unavailable".to_owned())?;
    let association_max_channels = if sctp.is_null() || sctp.is_undefined() {
        None
    } else {
        let capacity = Reflect::get(&sctp, &JsValue::from_str("maxChannels"))
            .map_err(|_| "browser SCTP capacity unavailable".to_owned())?;
        if capacity.is_null() || capacity.is_undefined() {
            None
        } else {
            let capacity = capacity
                .as_f64()
                .filter(|value| {
                    value.is_finite() && value.fract() == 0.0 && (1.0..=65_536.0).contains(value)
                })
                .ok_or_else(|| "invalid browser SCTP capacity".to_owned())?;
            Some(capacity as u32)
        }
    };
    Ok(agent_core::PreparedDataChannel {
        stream_id: channel
            .id()
            .ok_or_else(|| "browser channel stream ID unavailable".to_owned())?,
        negotiated: boolean_property("negotiated")?,
        ordered: boolean_property("ordered")?,
        reliability,
        association_max_channels,
    })
}

struct DataChannel {
    channel: RtcDataChannel,
    _open: Closure<dyn FnMut(Event)>,
    _close: Closure<dyn FnMut(Event)>,
    _error: Closure<dyn FnMut(Event)>,
    _message: Closure<dyn FnMut(MessageEvent)>,
}

impl DataChannel {
    fn close(self) {
        self.channel.set_onopen(None);
        self.channel.set_onclose(None);
        self.channel.set_onerror(None);
        self.channel.set_onmessage(None);
        self.channel.close();
    }
}

struct Peer {
    generation: Generation,
    connection: RtcPeerConnection,
    sdp: Option<agent_core::client_sdp::Executor>,
    transceivers: Vec<(MediaSlot, RtcRtpTransceiver)>,
    receiver_mids: BTreeMap<u32, String>,
    remote_tracks: BTreeMap<String, RemoteTrackState>,
    channels: BTreeMap<u64, DataChannel>,
    _state: Closure<dyn FnMut(Event)>,
    _track: Closure<dyn FnMut(RtcTrackEvent)>,
}

impl Peer {
    fn close(self) {
        self.connection.set_onconnectionstatechange(None);
        self.connection.set_ontrack(None);
        for track in self.remote_tracks.into_values() {
            track.close();
        }
        for channel in self.channels.into_values() {
            channel.close();
        }
        self.connection.close();
    }
}

struct RemoteTrackState {
    track: MediaStreamTrack,
    _ended: Closure<dyn FnMut(Event)>,
}

impl RemoteTrackState {
    fn close(self) {
        let _ = self
            .track
            .remove_event_listener_with_callback("ended", self._ended.as_ref().unchecked_ref());
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

    fn prepare_publication_state(
        &self,
        owner: u64,
        state: &agent_core::ffi::PublicationState,
        closing: bool,
    ) -> Option<agent_core::observer::PublicationNotification> {
        let inner = self.inner.upgrade()?;
        if closing {
            inner.prepare_pipe_close(owner);
        }
        inner.prepare_publication(owner, state, closing)
    }

    fn publication_claimed(
        &self,
        owner: u64,
        slot: &str,
        kind: MediaKind,
        after_generation: Option<Generation>,
    ) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        inner.register_publication(owner, slot, kind, after_generation);
    }

    fn resource_released(&self, owner: u64) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        inner.release_resource(owner);
    }

    fn prepare_resource_state(
        &self,
        owner: u64,
        state: &crate::engine::ResourceState,
        closing: bool,
    ) -> Option<crate::engine::ResourceNotification> {
        self.inner
            .upgrade()?
            .prepare_resource_state(owner, state, closing)
    }

    fn prepare_catalog_state(
        &self,
        owner: u64,
        state: &agent_core::ffi::WatchState,
        closing: bool,
    ) -> Option<crate::engine::ResourceNotification> {
        self.inner
            .upgrade()?
            .prepare_catalog_state(owner, state, closing)
    }

    fn publication_input_state(&self, state: &agent_core::PublicationInputState) {
        let Some(inner) = self.inner.upgrade() else {
            return;
        };
        inner.prepare_pipe_state(state);
        if !inner
            .publication_slots
            .borrow()
            .get(&state.owner)
            .is_some_and(|binding| binding.slot == state.slot)
        {
            return;
        }
        if let Some(input) = inner.local_tracks.borrow().get(&state.slot) {
            let current = input
                .input_operation
                .as_ref()
                .and_then(agent_core::PublicationInputOperation::input)
                == state.input;
            input.track.set_enabled(state.enabled && current);
        }
    }

    fn notify_prepared_pipe_states(&self) {
        if let Some(inner) = self.inner.upgrade() {
            inner.notify_prepared_pipe_states();
        }
    }
}

struct RuntimeInner {
    actor: RefCell<Option<ActorHandle>>,
    agent_state: Rc<agent_core::observer::AgentStateObserver>,
    publication_observers:
        RefCell<BTreeMap<u64, Rc<agent_core::observer::PublicationStateObserver>>>,
    resource_observers: RefCell<BTreeMap<u64, ResourceObserver>>,
    catalog_observers: RefCell<BTreeMap<u64, Rc<catalog::CatalogObserver>>>,
    pipe_observers: RefCell<
        Vec<(
            u64,
            agent_core::PublicationInputId,
            Weak<agent_core::observer::PipeStateObserver>,
        )>,
    >,
    pending_pipe_notifications: RefCell<Vec<agent_core::observer::PipeNotification>>,
    logger: BrowserLogger,
    local_operation_gates: RefCell<BTreeMap<String, Weak<LocalSlotOperations>>>,
    local_tracks: RefCell<BTreeMap<String, LocalTrackState>>,
    publication_slots: RefCell<BTreeMap<u64, PublicationSlot>>,
    observations: RefCell<media_observation::Observations>,
    #[cfg(feature = "uniffi")]
    data_reads: RefCell<std::collections::HashMap<u64, Option<std::task::Waker>>>,
    peers: RefCell<BTreeMap<u64, Peer>>,
    requests: RefCell<BTreeMap<u64, AbortController>>,
    timers: RefCell<BTreeMap<u64, TimerHandle>>,

    closing: Cell<bool>,
    closed: Cell<bool>,
}

pub(crate) struct BrowserRuntime {
    inner: Rc<RuntimeInner>,
}

impl BrowserRuntime {
    pub(crate) fn start(
        endpoint: String,
        token: String,
        log_level: LogLevel,
    ) -> Result<Self, agent_core::ffi::ErrorInfo> {
        let core_config = AgentConfig {
            endpoint,
            token,
            retry: RetryPolicy::default(),
            log_level: log_level.into(),
        };
        let inner = Rc::new(RuntimeInner {
            actor: RefCell::new(None),
            agent_state: Rc::new(agent_core::observer::AgentStateObserver::new(
                (&agent_core::Snapshot::default()).into(),
            )),
            publication_observers: RefCell::new(BTreeMap::new()),
            resource_observers: RefCell::new(BTreeMap::new()),
            catalog_observers: RefCell::new(BTreeMap::new()),
            pipe_observers: RefCell::new(Vec::new()),
            pending_pipe_notifications: RefCell::new(Vec::new()),
            logger: BrowserLogger::new(log_level),
            local_operation_gates: RefCell::new(BTreeMap::new()),
            local_tracks: RefCell::new(BTreeMap::new()),
            publication_slots: RefCell::new(BTreeMap::new()),
            observations: RefCell::new(media_observation::Observations::default()),
            #[cfg(feature = "uniffi")]
            data_reads: RefCell::default(),
            peers: RefCell::new(BTreeMap::new()),
            requests: RefCell::new(BTreeMap::new()),
            timers: RefCell::new(BTreeMap::new()),

            closing: Cell::new(false),
            closed: Cell::new(false),
        });
        let actor = spawn_actor(core_config, BrowserHost::new(&inner))
            .map_err(|error| agent_core::ffi::ErrorInfo::from(&error))?;
        *inner.actor.borrow_mut() = Some(actor);
        Ok(Self { inner })
    }

    pub(crate) fn join_room(&self, room: String) -> Result<(), agent_core::ffi::ErrorInfo> {
        let actor = self.inner.intent_actor()?;
        actor.join(room)?;
        self.inner.agent_state();
        Ok(())
    }

    pub(crate) fn renew_token(&self, token: String) -> Result<(), agent_core::ffi::ErrorInfo> {
        let actor = self.inner.intent_actor()?;
        actor.renew(token)?;
        self.inner.agent_state();
        Ok(())
    }

    pub(crate) fn close_locally(&self) {
        self.inner.shutdown();
        let actor = self.inner.actor.borrow_mut().take();
        if let Some(actor) = actor {
            let _ = actor.abort();
        }
    }

    #[cfg(feature = "uniffi")]
    pub(crate) fn claim_video(
        &self,
        selector: agent_core::TrackSelector,
        observation: agent_core::VideoObservation,
    ) -> Result<
        (
            u64,
            Rc<agent_core::observer::VideoSubscriptionStateObserver>,
        ),
        agent_core::ffi::ErrorInfo,
    > {
        use agent_core::ResourceCommand;
        self.claim_observed_resource(ResourceCommand::ClaimVideo { selector }, |owner| {
            self.configure_owned_video(owner, observation)?;
            self.video_state_observer(owner)
        })
    }

    #[cfg(feature = "uniffi")]
    pub(crate) fn claim_audio(
        &self,
        selector: agent_core::TrackSelector,
        playout_delay: agent_core::PlayoutDelay,
    ) -> Result<
        (
            u64,
            Rc<agent_core::observer::AudioSubscriptionStateObserver>,
        ),
        agent_core::ffi::ErrorInfo,
    > {
        use agent_core::ResourceCommand;
        self.claim_observed_resource(
            ResourceCommand::ClaimAudioSubscription { selector },
            |owner| {
                self.configure_owned_audio(owner, playout_delay)?;
                self.audio_state_observer(owner)
            },
        )
    }

    #[cfg(feature = "uniffi")]
    pub(crate) fn configure_owned_video(
        &self,
        owner: u64,
        observation: agent_core::VideoObservation,
    ) -> Result<(), agent_core::ffi::ErrorInfo> {
        self.apply_resource(agent_core::ResourceCommand::ObserveVideo { owner, observation })?;
        Ok(())
    }

    #[cfg(feature = "uniffi")]
    pub(crate) fn configure_owned_audio(
        &self,
        owner: u64,
        playout_delay: agent_core::PlayoutDelay,
    ) -> Result<(), agent_core::ffi::ErrorInfo> {
        self.apply_resource(agent_core::ResourceCommand::ConfigureAudio {
            owner,
            playout_delay,
        })?;
        Ok(())
    }

    #[cfg(feature = "uniffi")]
    pub(crate) fn claim_data_publication(
        &self,
        spec: agent_core::TrackSpec,
    ) -> Result<
        (u64, Rc<agent_core::observer::DataPublicationStateObserver>),
        agent_core::ffi::ErrorInfo,
    > {
        self.claim_observed_resource(
            agent_core::ResourceCommand::DeclareDataPublication { spec },
            |owner| self.data_publication_state_observer(owner),
        )
    }

    #[cfg(feature = "uniffi")]
    pub(crate) fn claim_data_subscription(
        &self,
        spec: agent_core::TrackSpec,
        source: agent_core::TrackSelector,
    ) -> Result<
        (u64, Rc<agent_core::observer::DataSubscriptionStateObserver>),
        agent_core::ffi::ErrorInfo,
    > {
        self.claim_observed_resource(
            agent_core::ResourceCommand::ReceiveData { spec, source },
            |owner| self.data_subscription_state_observer(owner),
        )
    }

    #[cfg(feature = "uniffi")]
    fn claim_observed_resource<T>(
        &self,
        command: agent_core::ResourceCommand,
        observe: impl FnOnce(u64) -> Result<T, agent_core::ffi::ErrorInfo>,
    ) -> Result<(u64, T), agent_core::ffi::ErrorInfo> {
        use agent_core::{ResourceCommand, ResourceResult, ffi};
        let ResourceResult::Owned { owner } = self.apply_resource(command)? else {
            return Err(ffi::ErrorInfo::new(
                ffi::ErrorCode::Internal,
                "Unexpected resource declaration result",
            ));
        };
        let result = observe(owner);
        if result.is_err() {
            let _ = self.apply_resource(ResourceCommand::Release { owner });
        }
        result.map(|observer| (owner, observer))
    }

    #[cfg(feature = "uniffi")]
    pub(crate) async fn observe_exact_media(
        &self,
        owner: u64,
    ) -> Result<media_observation::BrowserMediaObservation, JsValue> {
        self.inner.observe_cancellable(owner).await
    }

    #[cfg(feature = "uniffi")]
    pub(crate) fn release_media_owner(&self, owner: u64) -> Result<(), agent_core::ffi::ErrorInfo> {
        if !self.inner.is_closed() {
            self.apply_resource(agent_core::ResourceCommand::Release { owner })?;
        }
        Ok(())
    }

    #[cfg(feature = "uniffi")]
    pub(crate) fn current_state(&self) -> agent_core::ffi::AgentState {
        self.inner.agent_state()
    }

    #[cfg(feature = "uniffi")]
    pub(crate) fn open_updates(
        &self,
    ) -> Result<agent_core::observer::AgentUpdates, agent_core::observer::ObserverLimit> {
        self.inner.agent_state();
        self.inner.agent_state.updates()
    }
}

impl BrowserRuntime {
    pub(crate) fn configure_logging(
        &self,
        level: LogLevel,
    ) -> Result<(), agent_core::ffi::ErrorInfo> {
        use agent_core::ffi::{ErrorCode, ErrorInfo};
        self.inner
            .intent_actor()?
            .send_public(PublicCommand::SetLogLevel(level.into()))
            .map_err(|_| {
                ErrorInfo::new(ErrorCode::CapacityExceeded, "Log update mailbox is full")
            })?;
        self.inner.logger.set_level(level);
        Ok(())
    }

    fn apply_resource(
        &self,
        command: agent_core::ResourceCommand,
    ) -> Result<agent_core::ResourceResult, agent_core::ffi::ErrorInfo> {
        let actor = self.inner.intent_actor()?;
        let release = match &command {
            agent_core::ResourceCommand::Release { owner } => Some(*owner),
            _ => None,
        };
        let final_state = release.and_then(|owner| actor.publication_state(owner));
        let final_resource = release.and_then(|owner| actor.observed_resource_state(owner));
        let result = actor.resource_now(command);
        let mut publications = Vec::new();
        let mut resources = Vec::new();
        if let Some(owner) = release {
            self.inner.release_resource(owner);
            if let Some(mut state) = final_state {
                state.closed = true;
                publications.extend(self.inner.prepare_publication(owner, &state, true));
            }
            if let Some(mut state) = final_resource {
                state.mark_closed();
                resources.extend(self.inner.prepare_resource_state(owner, &state, true));
            }
        }
        let owners = self
            .inner
            .publication_observers
            .borrow()
            .keys()
            .copied()
            .collect::<Vec<_>>();
        publications.extend(
            owners
                .into_iter()
                .filter_map(|owner| self.inner.prepare_current_publication(owner)),
        );
        resources.extend(self.inner.prepare_current_resources());
        for notification in publications {
            notification.notify();
        }
        for notification in resources {
            notification();
        }
        self.inner.notify_prepared_pipe_states();
        result
    }
}

impl BrowserRuntime {
    pub(crate) fn attach_publication_pipe(
        &self,
        owner: u64,
    ) -> Result<BrowserPipe, agent_core::ffi::ErrorInfo> {
        use agent_core::{ResourceCommand, ResourceResult, ffi};
        let actor = self.inner.intent_actor()?;
        let internal = |message| ffi::ErrorInfo::new(ffi::ErrorCode::Internal, message);
        let result = actor.resource_now(ResourceCommand::AttachPublicationInput { owner })?;
        let ResourceResult::PublicationInput {
            operation,
            state: _,
        } = result
        else {
            return Err(internal("Core did not reserve a publication input"));
        };
        let input = operation
            .input()
            .ok_or_else(|| internal("Core did not allocate a publication input ID"))?;
        let initial = actor
            .pipe_state(input)
            .ok_or_else(|| internal("Core pipe state is unavailable"))?;
        let observer = Rc::new(agent_core::observer::PipeStateObserver::new(initial));
        self.inner
            .pipe_observers
            .borrow_mut()
            .push((owner, input, Rc::downgrade(&observer)));
        let pipe = BrowserPipe {
            actor,
            input,
            observer,
            closed: Cell::new(false),
            owner,
            slot: operation.slot().into(),
            inner: Rc::downgrade(&self.inner),
            closing: RefCell::new(None),
        };
        if let Some(notification) = self.inner.prepare_current_publication(owner) {
            notification.notify();
        }
        Ok(pipe)
    }

    pub(crate) fn configure_publication_state(
        &self,
        owner: u64,
        enabled: bool,
    ) -> Result<(), agent_core::ffi::ErrorInfo> {
        let actor = self.inner.intent_actor()?;
        let result = actor.resource_now(agent_core::ResourceCommand::SetPublicationMuted {
            owner,
            muted: !enabled,
        })?;
        let host = BrowserHost::new(&self.inner);
        if let agent_core::ResourceResult::PublicationInputState(state) = result {
            host.publication_input_state(&state);
        }
        if let Some(notification) = self.inner.prepare_current_publication(owner) {
            notification.notify();
        }
        host.notify_prepared_pipe_states();
        Ok(())
    }

    pub(crate) fn claim_media_publication(
        &self,
        spec: agent_core::TrackSpec,
    ) -> Result<agent_core::ResourceResult, agent_core::ffi::ErrorInfo> {
        use agent_core::{ResourceCommand, ResourceResult, ffi};
        let actor = self.inner.intent_actor()?;
        let kind = spec.media_kind().ok_or_else(|| {
            ffi::ErrorInfo::new(
                ffi::ErrorCode::InvalidArgument,
                "Expected a media publication declaration",
            )
        })?;
        let result = actor.resource_now(ResourceCommand::ClaimPublication { spec })?;
        let ResourceResult::Publication {
            owner,
            slot,
            after_generation,
        } = &result
        else {
            return Err(ffi::ErrorInfo::new(
                ffi::ErrorCode::Internal,
                "Unexpected publication declaration result",
            ));
        };
        self.inner
            .register_publication(*owner, slot, kind, *after_generation);
        if let Some(notification) = self.inner.prepare_current_publication(*owner) {
            notification.notify();
        }
        Ok(result)
    }

    pub(crate) fn publication_state_observer(
        &self,
        owner: u64,
    ) -> Result<Rc<agent_core::observer::PublicationStateObserver>, agent_core::ffi::ErrorInfo>
    {
        use agent_core::ffi;
        self.inner
            .publication_observers
            .borrow()
            .get(&owner)
            .cloned()
            .ok_or_else(|| {
                ffi::ErrorInfo::new(ffi::ErrorCode::Closed, "Publication owner is not active")
            })
    }
}

impl Drop for BrowserRuntime {
    fn drop(&mut self) {
        self.close_locally();
    }
}

impl RuntimeInner {
    fn is_closed(&self) -> bool {
        self.closing.get() || self.closed.get()
    }

    fn ensure_open(&self) -> Result<(), JsValue> {
        if self.is_closed() {
            Err(js_error("browser runtime is closed"))
        } else {
            Ok(())
        }
    }

    fn intent_actor(&self) -> Result<ActorHandle, agent_core::ffi::ErrorInfo> {
        let closed = || {
            agent_core::ffi::ErrorInfo::new(
                agent_core::ffi::ErrorCode::Closed,
                "Browser runtime is closed",
            )
        };
        if self.is_closed() {
            return Err(closed());
        }
        self.actor.borrow().clone().ok_or_else(closed)
    }

    fn ensure_local_operation_open(&self) -> Result<(), LocalOperationError> {
        if self.is_closed() {
            Err(LocalOperationError::runtime("browser runtime is closed"))
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

    fn register_publication(
        self: &Rc<Self>,
        owner: u64,
        slot: &str,
        kind: MediaKind,
        after_generation: Option<Generation>,
    ) {
        let operations = self.local_operations(slot);
        self.publication_slots.borrow_mut().insert(
            owner,
            PublicationSlot {
                slot: slot.into(),
                kind,
                operations,
                after_generation,
            },
        );
    }

    fn snapshot(&self) -> agent_core::Snapshot {
        let actor = self.actor.borrow();
        actor
            .as_ref()
            .map_or_else(agent_core::Snapshot::default, |actor| actor.snapshot())
    }

    fn agent_state(&self) -> agent_core::ffi::AgentState {
        let snapshot = self.snapshot();
        let _ = self
            .agent_state
            .publish_from(snapshot.version, (&snapshot).into());
        self.agent_state.state()
    }

    fn close_publication_observers(&self) {
        self.prepare_pipe_shutdown();
        let resources = self.prepare_resource_shutdown();
        let catalogs = self.prepare_catalog_shutdown();
        let mut agent_state: agent_core::ffi::AgentState = (&self.snapshot()).into();
        agent_state.room = None;
        let publications = std::mem::take(&mut *self.publication_observers.borrow_mut());
        let current = {
            let actor = self.actor.borrow();
            publications
                .keys()
                .map(|owner| {
                    (
                        *owner,
                        actor
                            .as_ref()
                            .and_then(|actor| actor.publication_state(*owner)),
                    )
                })
                .collect::<BTreeMap<_, _>>()
        };
        let notifications = publications
            .into_iter()
            .filter_map(|(owner, observer)| {
                let state = current
                    .get(&owner)
                    .and_then(Clone::clone)
                    .unwrap_or_else(|| observer.state());
                observer.prepare(state, true).ok()
            })
            .collect::<Vec<_>>();
        let _ = self.agent_state.close(agent_state);
        for notification in notifications {
            notification.notify();
        }
        for notification in resources {
            notification();
        }
        for notification in catalogs {
            notification();
        }
        self.notify_prepared_pipe_states();
        #[cfg(feature = "uniffi")]
        self.wake_data_reads();
    }

    fn prepare_pipe_state(&self, input_state: &agent_core::PublicationInputState) {
        let observers = self
            .pipe_observers
            .borrow()
            .iter()
            .filter(|(owner, _, _)| *owner == input_state.owner)
            .filter_map(|(_, input, observer)| {
                observer.upgrade().map(|observer| (*input, observer))
            })
            .collect::<Vec<_>>();
        let mut notifications = self.pending_pipe_notifications.borrow_mut();
        for (input, observer) in observers {
            let state = self
                .actor
                .borrow()
                .as_ref()
                .and_then(|actor| actor.pipe_state(input));
            let closed = state.is_none();
            if let Ok(notification) =
                observer.prepare(state.unwrap_or_else(|| observer.state()), closed)
            {
                notifications.push(notification);
            }
            if closed {
                self.forget_pipe(input);
            }
        }
    }

    fn notify_prepared_pipe_states(&self) {
        let notifications = std::mem::take(&mut *self.pending_pipe_notifications.borrow_mut());
        for notification in notifications {
            notification.notify();
        }
    }

    fn prepare_pipe_close(&self, owner: u64) {
        let observers = self
            .pipe_observers
            .borrow()
            .iter()
            .filter(|(registered_owner, _, _)| *registered_owner == owner)
            .filter_map(|(_, _, observer)| observer.upgrade())
            .collect::<Vec<_>>();
        let mut notifications = self.pending_pipe_notifications.borrow_mut();
        for observer in observers {
            if let Ok(notification) = observer.prepare(observer.state(), true) {
                notifications.push(notification);
            }
        }
    }

    fn close_pipe_observers_for(&self, owner: u64) {
        let mut registry = self.pipe_observers.borrow_mut();
        let mut retained = Vec::with_capacity(registry.len());
        let mut closing = Vec::new();
        for (registered_owner, input, observer) in registry.drain(..) {
            if registered_owner == owner {
                if let Some(observer) = observer.upgrade() {
                    closing.push((input, observer));
                }
            } else if observer.strong_count() > 0 {
                retained.push((registered_owner, input, observer));
            }
        }
        *registry = retained;
        drop(registry);
        for (_, observer) in closing {
            if let Ok(notification) = observer.prepare(observer.state(), true) {
                notification.notify();
            }
        }
    }

    fn prepare_pipe_shutdown(&self) {
        let pipes = std::mem::take(&mut *self.pipe_observers.borrow_mut());
        let mut notifications = self.pending_pipe_notifications.borrow_mut();
        for (_, input, observer) in pipes {
            if let Some(observer) = observer.upgrade() {
                let state = self
                    .actor
                    .borrow()
                    .as_ref()
                    .and_then(|actor| actor.pipe_state(input))
                    .unwrap_or_else(|| observer.state());
                if let Ok(notification) = observer.prepare(state, true) {
                    notifications.push(notification);
                }
            }
        }
    }

    fn forget_pipe(&self, input: agent_core::PublicationInputId) {
        self.pipe_observers
            .borrow_mut()
            .retain(|(_, id, observer)| *id != input && observer.strong_count() > 0);
    }

    fn fence_pipe_input(&self, input: agent_core::PublicationInputId, slot: &str) {
        if let Some(track) = self.local_tracks.borrow().get(slot).filter(|track| {
            track
                .input_operation
                .as_ref()
                .is_some_and(|operation| operation.input() == Some(input))
        }) {
            track.track.set_enabled(false);
        }
    }

    async fn cleanup_pipe_input(
        self: &Rc<Self>,
        input: agent_core::PublicationInputId,
        slot: &str,
    ) -> Result<(), LocalOperationError> {
        let operations = self.local_operations(slot);
        let _permit = operations.gate.enter().await;
        let clear = {
            let mut tracks = self.local_tracks.borrow_mut();
            if tracks.get(slot).is_some_and(|track| {
                track
                    .input_operation
                    .as_ref()
                    .is_some_and(|operation| operation.input() == Some(input))
            }) {
                if let Some(track) = tracks.remove(slot) {
                    track.track.stop();
                }
                true
            } else {
                !tracks.contains_key(slot)
            }
        };
        if clear && !self.is_closed() {
            self.sync_local_slot(slot, None)
                .await
                .map_err(LocalOperationError::runtime)?;
        }
        Ok(())
    }

    async fn input_resource(
        &self,
        command: agent_core::ResourceCommand,
    ) -> Result<agent_core::ResourceResult, LocalOperationError> {
        let actor = self
            .actor
            .borrow()
            .clone()
            .ok_or_else(|| LocalOperationError::runtime("runtime is closed"))?;
        actor
            .send_resource(command)
            .await
            .map_err(LocalOperationError::runtime)?
            .await
            .map_err(|_| LocalOperationError::runtime("input operation canceled"))?
            .map_err(|error| LocalOperationError::runtime(error.to_string()))
    }

    async fn input_current(
        &self,
        operation: &agent_core::PublicationInputOperation,
    ) -> Result<bool, LocalOperationError> {
        Ok(matches!(
            self.input_resource(agent_core::ResourceCommand::AuthorizePublicationInput {
                operation: operation.clone(),
            })
            .await?,
            agent_core::ResourceResult::Updated
        ))
    }

    fn discard_input(&self, operation: &agent_core::PublicationInputOperation) {
        let mut inputs = self.local_tracks.borrow_mut();
        if inputs
            .get(operation.slot())
            .is_some_and(|input| input.input_operation.as_ref() == Some(operation))
        {
            inputs.remove(operation.slot()).unwrap().track.stop();
        }
    }

    fn local_operations(self: &Rc<Self>, slot: &str) -> Rc<LocalSlotOperations> {
        let mut gates = self.local_operation_gates.borrow_mut();
        if let Some(operations) = gates.get(slot).and_then(Weak::upgrade) {
            return operations;
        }
        // Reclaimed coordinates share any unfinished physical sender I/O.
        let operations = Rc::new(LocalSlotOperations {
            slot: slot.into(),
            inner: Rc::downgrade(self),
            gate: LocalOperationGate::default(),
        });
        gates.insert(slot.into(), Rc::downgrade(&operations));
        operations
    }

    fn invalidate_local_slot(self: &Rc<Self>, binding: PublicationSlot) {
        let slot = binding.slot;
        if let Some(state) = self.local_tracks.borrow_mut().remove(&slot) {
            state.track.stop();
        }
        if self.peers.borrow().is_empty() {
            return;
        }
        let weak = Rc::downgrade(self);
        spawn_local(async move {
            let _permit = binding.operations.gate.enter().await;
            let Some(inner) = weak.upgrade() else { return };
            if inner.is_closed()
                || inner
                    .publication_slots
                    .borrow()
                    .values()
                    .any(|binding| binding.slot == slot)
            {
                return;
            }
            if let Err(error) = inner.sync_local_slot(&slot, None).await {
                inner.report_error(error);
            }
        });
    }

    async fn replace_owned_input(
        self: &Rc<Self>,
        track: Option<MediaStreamTrack>,
        config: SenderConfig,
        publication: PublicationInputRequest,
    ) -> Result<(), LocalOperationError> {
        self.ensure_local_operation_open()?;
        let binding = self
            .publication_slots
            .borrow()
            .get(&publication.owner)
            .cloned()
            .ok_or_else(|| LocalOperationError::runtime("publication owner was released"))?;
        let slot = &binding.slot;
        let config = if track.is_some() {
            normalize_sender_config(binding.kind, config)
                .map_err(LocalOperationError::validation)?
        } else {
            config
        };
        if let Some(track) = &track {
            let expected = media_kind_name(binding.kind);
            if track.kind() != expected {
                return Err(LocalOperationError::validation(format!(
                    "publication requires a {expected} track, received {}",
                    track.kind()
                )));
            }
            if !input_is_live(track) {
                return Err(LocalOperationError::validation("capture input has ended"));
            }
        }
        let agent_core::ResourceResult::PublicationInput { operation, .. } = self
            .input_resource(agent_core::ResourceCommand::BeginPublicationInput {
                owner: publication.owner,
                input: publication.intent,
                present: track.is_some(),
                muted: publication.muted,
            })
            .await?
        else {
            return Err(LocalOperationError::runtime(
                "invalid Core input instruction",
            ));
        };
        if !self.input_current(&operation).await? {
            return Ok(());
        }
        let _permit = binding.operations.gate.enter().await;
        self.ensure_local_operation_open()?;
        if !self.input_current(&operation).await? {
            return Ok(());
        }
        let previous = if let Some(track) = &track {
            let source_id = track.id();
            let track = MediaStreamTrack::clone(track);
            track.set_enabled(false);
            if let Err(error) = set_property(
                track.as_ref(),
                "contentHint",
                &JsValue::from_str(&config.content_hint),
            ) {
                track.stop();
                if self
                    .input_resource(agent_core::ResourceCommand::CompletePublicationInput {
                        operation: operation.clone(),
                        result: Err(agent_core::ffi::ErrorCode::Internal),
                    })
                    .await?
                    == agent_core::ResourceResult::Superseded
                {
                    return Ok(());
                }
                return Err(LocalOperationError::runtime(error));
            }
            self.local_tracks.borrow_mut().insert(
                slot.clone(),
                LocalTrackState {
                    track,
                    config,
                    source_id,
                    input_operation: Some(operation.clone()),
                    after_generation: binding.after_generation,
                },
            )
        } else {
            self.local_tracks.borrow_mut().remove(slot)
        };
        if let Some(previous) = previous {
            previous.track.stop();
        }
        let mut result = self
            .sync_local_slot(slot, Some(&operation))
            .await
            .map_err(LocalOperationError::runtime);
        if result.is_ok()
            && self.local_tracks.borrow().get(slot).is_some_and(|input| {
                input.input_operation.as_ref() == Some(&operation) && !input_is_live(&input.track)
            })
        {
            result = Err(LocalOperationError::runtime(
                "capture input ended during registration",
            ));
        }
        let completed = self
            .input_resource(agent_core::ResourceCommand::CompletePublicationInput {
                operation: operation.clone(),
                result: result
                    .as_ref()
                    .map(|_| ())
                    .map_err(|error| error.error_info().code),
            })
            .await?;
        if completed == agent_core::ResourceResult::Superseded {
            self.discard_input(&operation);
            return Ok(());
        }
        if result.is_err() {
            self.discard_input(&operation);
        }
        result?;
        self.ensure_local_operation_open()
    }

    async fn sync_local_slot(
        self: &Rc<Self>,
        slot: &str,
        operation: Option<&agent_core::PublicationInputOperation>,
    ) -> Result<(), String> {
        let state = self.local_tracks.borrow().get(slot).cloned();
        let operation = operation.or_else(|| state.as_ref()?.input_operation.as_ref());
        let senders: Vec<(Generation, RtcRtpSender)> = self
            .peers
            .borrow()
            .values()
            .flat_map(|peer| {
                peer.transceivers
                    .iter()
                    .filter_map(move |(media_slot, transceiver)| match media_slot {
                        MediaSlot::LocalVideo(name) | MediaSlot::LocalAudio(name)
                            if name == slot =>
                        {
                            Some((peer.generation, transceiver.sender()))
                        }
                        _ => None,
                    })
            })
            .collect();
        for (generation, sender) in senders {
            if let Some(operation) = operation {
                if !self
                    .input_current(operation)
                    .await
                    .map_err(|error| error.message)?
                {
                    return Ok(());
                }
            }
            let eligible = state.as_ref().filter(|state| {
                state
                    .after_generation
                    .is_none_or(|after| generation > after)
            });
            let result = apply_sender_state(&self.logger, &sender, eligible).await;
            if !self.peers.borrow().contains_key(&generation.get()) {
                continue;
            }
            if let Some(operation) = operation {
                if !self
                    .input_current(operation)
                    .await
                    .map_err(|error| error.message)?
                {
                    return Ok(());
                }
            }
            result?;
        }
        Ok(())
    }

    fn release_resource(self: &Rc<Self>, owner: u64) {
        #[cfg(feature = "uniffi")]
        {
            let wake = self
                .data_reads
                .borrow_mut()
                .get_mut(&owner)
                .and_then(Option::take);
            if let Some(wake) = wake {
                wake.wake();
            }
        }
        self.close_pipe_observers_for(owner);
        let binding = self.publication_slots.borrow_mut().remove(&owner);
        if let Some(binding) = binding {
            self.invalidate_local_slot(binding);
        }
    }

    fn prepare_current_publication(
        &self,
        owner: u64,
    ) -> Option<agent_core::observer::PublicationNotification> {
        let state = self
            .actor
            .borrow()
            .as_ref()
            .and_then(|actor| actor.publication_state(owner))?;
        self.prepare_publication(owner, &state, false)
    }

    fn prepare_publication(
        &self,
        owner: u64,
        state: &agent_core::ffi::PublicationState,
        closing: bool,
    ) -> Option<agent_core::observer::PublicationNotification> {
        if self.is_closed() && !closing {
            return None;
        }
        let observer = {
            let mut observers = self.publication_observers.borrow_mut();
            observers
                .entry(owner)
                .or_insert_with(|| {
                    Rc::new(agent_core::observer::PublicationStateObserver::new(
                        state.clone(),
                    ))
                })
                .clone()
        };
        let notification = observer.prepare(state.clone(), closing).ok();
        if closing {
            self.publication_observers.borrow_mut().remove(&owner);
        }
        notification
    }

    #[cfg(feature = "uniffi")]
    fn wake_data_reads(&self) {
        let wakes: Vec<_> = self
            .data_reads
            .borrow_mut()
            .values_mut()
            .filter_map(Option::take)
            .collect();
        for wake in wakes {
            wake.wake();
        }
    }

    fn publish_turn(&self, turn: &Turn) {
        #[cfg(feature = "uniffi")]
        self.wake_data_reads();
        if turn.snapshot.is_some() {
            self.agent_state();
        }
        if let Some(error) = &turn.error {
            let class = match error.source {
                TurnErrorSource::Command => "validation",
                TurnErrorSource::HostEvent => "protocol",
            };
            self.report_error(format!("core rejected {class} input: {error}"));
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
            Effect::PublicationInput(control) => {
                BrowserHost::new(self).publication_input_state(&control.state());
            }
            Effect::MediaObservation(control) => self.observation_control(control),
        }
    }

    fn execute_rtc(self: &Rc<Self>, effect: RtcEffect) {
        match effect {
            RtcEffect::CreateOffer {
                generation,
                data_channels,
            } => match self.build_peer(generation, &data_channels) {
                Ok(peer) => {
                    let previous = self.peers.borrow_mut().insert(generation.get(), peer);
                    debug_assert!(previous.is_none(), "generation must own one peer");
                    let inner = Rc::clone(self);
                    spawn_local(async move {
                        inner.create_offer(generation);
                    });
                }
                Err(error) => self.rtc_failed(generation, error),
            },
            RtcEffect::ApplyAnswer {
                generation,
                answer,
                capabilities,
            } => {
                self.apply_answer(generation, answer, capabilities);
            }
            RtcEffect::CreateDataChannel {
                generation,
                channel,
                specification,
            } => {
                let connection = {
                    let peers = self.peers.borrow();
                    peers
                        .get(&generation.get())
                        .filter(|peer| !peer.channels.contains_key(&channel.get()))
                        .map(|peer| peer.connection.clone())
                };
                let result = connection
                    .filter(|_| {
                        specification
                            .negotiated_id
                            .is_some_and(|id| id != 0 && id != u16::MAX)
                    })
                    .ok_or_else(|| "payload channel requires an available identity and negotiated non-control stream".to_owned())
                    .and_then(|connection| {
                        self.create_data_channel(generation, channel, &connection, &specification)
                    });
                match result {
                    Ok(created) => {
                        if let Some(peer) = self.peers.borrow_mut().get_mut(&generation.get()) {
                            let previous = peer.channels.insert(channel.get(), created);
                            debug_assert!(previous.is_none(), "channel identity must be unique");
                        } else {
                            created.close();
                        }
                    }
                    Err(error) => {
                        self.send_event(HostEvent::DataChannel(DataChannelEvent::Prepared {
                            generation,
                            channel,
                            preparation: Err(error),
                        }));
                    }
                }
            }
            RtcEffect::CloseDataChannel {
                generation,
                channel,
            } => {
                let channel = self
                    .peers
                    .borrow()
                    .get(&generation.get())
                    .and_then(|peer| peer.channels.get(&channel.get()))
                    .map(|channel| channel.channel.clone());
                if let Some(channel) = channel.filter(|channel| channel.id() != Some(0)) {
                    channel.close();
                }
            }
            RtcEffect::PrepareMedia {
                generation,
                operation,
                requests,
            } => {
                self.prepare_media(generation, operation, requests);
            }
            RtcEffect::AllocateMedia {
                generation,
                operation,
                allocations,
            } => {
                self.allocate_media(generation, operation, allocations);
            }
            RtcEffect::RetireMedia {
                generation,
                operation,
                mids,
            } => {
                self.retire_media(generation, operation, mids);
            }
            RtcEffect::CancelMediaPreparation {
                generation,
                operation,
            } => {
                self.cancel_media(generation, operation);
            }
            RtcEffect::Close { generation } => {
                let peer = self.peers.borrow_mut().remove(&generation.get());
                if let Some(peer) = peer {
                    peer.close();
                }
                self.retire_observation_tracks();
                self.send_event(HostEvent::Rtc(RtcEvent::Closed { generation }));
            }
        }
    }

    fn build_peer(
        self: &Rc<Self>,
        generation: Generation,
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
            inner.retire_observation_tracks();
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
            let remote_track = event.track();
            let ended_weak = Rc::downgrade(&inner);
            let ended_mid = mid.clone();
            let ended_track = Clone::clone(&remote_track);
            let ended = Closure::wrap(Box::new(move |_event: Event| {
                let Some(inner) = ended_weak.upgrade() else {
                    return;
                };
                let removed = inner
                    .peers
                    .borrow_mut()
                    .get_mut(&generation.get())
                    .and_then(|peer| {
                        let current = peer.remote_tracks.get(&ended_mid)?;
                        if !same_js_object(&current.track, &ended_track) {
                            return None;
                        }
                        peer.remote_tracks.remove(&ended_mid)
                    });
                if let Some(removed) = removed {
                    removed.close();
                    inner.retire_observation_tracks();
                    inner.agent_state();
                }
            }) as Box<dyn FnMut(Event)>);
            if let Err(error) = remote_track
                .add_event_listener_with_callback("ended", ended.as_ref().unchecked_ref())
            {
                inner.rtc_failed(
                    generation,
                    format!("could not observe remote track end: {}", js_message(error)),
                );
                return;
            }
            let state = RemoteTrackState {
                track: remote_track,
                _ended: ended,
            };
            let (inserted, previous, orphan) = {
                let mut peers = inner.peers.borrow_mut();
                if let Some(peer) = peers.get_mut(&generation.get()) {
                    (true, peer.remote_tracks.insert(mid, state), None)
                } else {
                    (false, None, Some(state))
                }
            };
            if let Some(previous) = previous {
                previous.close();
            }
            if let Some(orphan) = orphan {
                orphan.close();
            }
            if inserted {
                inner.retire_observation_tracks();
                inner.agent_state();
            }
        }) as Box<dyn FnMut(RtcTrackEvent)>);
        connection.set_ontrack(Some(track.as_ref().unchecked_ref()));

        let mut channels = BTreeMap::<u64, DataChannel>::new();
        for (index, spec) in specs.iter().enumerate() {
            let numeric = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
            let Some(channel_id) = ChannelId::new(numeric) else {
                return Err("data channel ID space exhausted".into());
            };
            let channel = match self.create_data_channel(generation, channel_id, &connection, spec)
            {
                Ok(channel) => channel,
                Err(error) => {
                    connection.set_onconnectionstatechange(None);
                    connection.set_ontrack(None);
                    for channel in channels.into_values() {
                        channel.close();
                    }
                    connection.close();
                    return Err(error);
                }
            };
            let previous = channels.insert(channel_id.get(), channel);
            debug_assert!(previous.is_none(), "channel IDs must be unique");
        }

        Ok(Peer {
            generation,
            connection,
            sdp: None,
            // Keep the initial SCTP m-line as the non-retiring BUNDLE anchor.
            transceivers: Vec::new(),
            receiver_mids: BTreeMap::new(),
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
        if let Some(id) = spec.negotiated_id {
            init.set_negotiated(true);
            init.set_id(id);
        }
        let expected_limits = match spec.reliability {
            DataChannelReliability::Reliable => (None, None),
            DataChannelReliability::MaxRetransmits(value) => {
                init.set_max_retransmits(value);
                (Some(value), None)
            }
            DataChannelReliability::MaxPacketLifetime(value) => {
                init.set_max_packet_life_time(value);
                (None, Some(value))
            }
        };
        let association = Reflect::get(connection.as_ref(), &JsValue::from_str("sctp"))
            .unwrap_or(JsValue::UNDEFINED);
        let channel = connection.create_data_channel_with_data_channel_dict(&spec.label, &init);
        if let Some(id) = spec.negotiated_id {
            // Pinned web-sys omits these standard RTCDataChannel getters.
            let boolean_property = |name: &str| {
                Reflect::get(channel.as_ref(), &JsValue::from_str(name))
                    .ok()
                    .and_then(|value| value.as_bool())
            };
            if channel.id() != Some(id)
                || boolean_property("negotiated") != Some(true)
                || boolean_property("ordered") != Some(spec.ordered)
                || (channel.max_retransmits(), channel.max_packet_life_time()) != expected_limits
            {
                channel.close();
                return Err("browser negotiated data channel configuration mismatch".to_owned());
            }
        }
        channel.set_binary_type(RtcDataChannelType::Arraybuffer);

        let open_weak = Rc::downgrade(self);
        let open_channel = channel.clone();
        let open = Closure::wrap(Box::new(move |_event: Event| {
            if let Some(inner) = open_weak.upgrade() {
                let preparation = {
                    let peers = inner.peers.borrow();
                    let Some(peer) = peers.get(&generation.get()) else {
                        return;
                    };
                    let Some(record) = peer.channels.get(&channel_id.get()).filter(|record| {
                        Object::is(record.channel.as_ref(), open_channel.as_ref())
                            && record.channel.ready_state() == RtcDataChannelState::Open
                    }) else {
                        return;
                    };
                    prepared_data_channel(&peer.connection, &record.channel)
                };
                inner.send_event(HostEvent::DataChannel(DataChannelEvent::Prepared {
                    generation,
                    channel: channel_id,
                    preparation,
                }));
                inner.send_event(HostEvent::DataChannel(DataChannelEvent::Opened {
                    generation,
                    channel: channel_id,
                }));
            }
        }) as Box<dyn FnMut(Event)>);
        channel.set_onopen(Some(open.as_ref().unchecked_ref()));

        let failed = Rc::new(Cell::new(false));
        let error_failed = failed.clone();
        let error = Closure::wrap(
            Box::new(move |_event: Event| error_failed.set(true)) as Box<dyn FnMut(Event)>
        );
        channel.set_onerror(Some(error.as_ref().unchecked_ref()));

        let close_weak = Rc::downgrade(self);
        let close_channel = channel.clone();
        let close = Closure::wrap(Box::new(move |_event: Event| {
            if let Some(inner) = close_weak.upgrade() {
                let (retired, reset_complete) = {
                    let mut peers = inner.peers.borrow_mut();
                    let Some(peer) = peers.get_mut(&generation.get()) else {
                        return;
                    };
                    if !peer.channels.get(&channel_id.get()).is_some_and(|record| {
                        Object::is(record.channel.as_ref(), close_channel.as_ref())
                            && record.channel.ready_state() == RtcDataChannelState::Closed
                    }) {
                        return;
                    }
                    let reset_complete = !failed.get()
                        && Reflect::get(peer.connection.as_ref(), &JsValue::from_str("sctp"))
                            .ok()
                            .is_some_and(|current| Object::is(&current, &association))
                        && Reflect::get(&association, &JsValue::from_str("state"))
                            .ok()
                            .and_then(|state| state.as_string())
                            .as_deref()
                            == Some("connected");
                    (peer.channels.remove(&channel_id.get()), reset_complete)
                };
                // Keep this executing closure alive until its callback returns.
                if let Some(retired) = retired {
                    spawn_local(async move { retired.close() });
                }
                inner.send_event(HostEvent::DataChannel(DataChannelEvent::Closed {
                    generation,
                    channel: channel_id,
                    reset_complete,
                }));
            }
        }) as Box<dyn FnMut(Event)>);
        channel.set_onclose(Some(close.as_ref().unchecked_ref()));

        let message_weak = Rc::downgrade(self);
        let message_channel = channel.clone();
        let message = Closure::wrap(Box::new(move |event: MessageEvent| {
            let Some(inner) = message_weak.upgrade() else {
                return;
            };
            if !inner
                .peers
                .borrow()
                .get(&generation.get())
                .is_some_and(|peer| {
                    peer.channels.get(&channel_id.get()).is_some_and(|record| {
                        Object::is(record.channel.as_ref(), message_channel.as_ref())
                            && record.channel.ready_state() == RtcDataChannelState::Open
                    })
                })
            {
                return;
            }
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
            _error: error,
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
                    Ok(offer) => match inner.offer_resources(generation, &offer) {
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

    fn offer_resources(
        self: &Rc<Self>,
        generation: Generation,
        offer: &str,
    ) -> Result<OfferResources, String> {
        let mut peers = self.peers.borrow_mut();
        let peer = peers
            .get_mut(&generation.get())
            .ok_or_else(|| "offer completed for an obsolete generation".to_owned())?;
        let mut slot_mids = Vec::with_capacity(peer.transceivers.len());
        for (slot, transceiver) in &peer.transceivers {
            let mid = transceiver
                .mid()
                .ok_or_else(|| "browser did not assign a MID after local description".to_owned())?;
            slot_mids.push((slot.clone(), mid));
        }
        let slots = agent_core::negotiated_slot_bindings(offer, slot_mids)?;
        peer.receiver_mids = slots
            .iter()
            .filter(|binding| binding.direction == agent_core::MediaDirection::ReceiveOnly)
            .map(|binding| (binding.media_index, binding.mid.clone()))
            .collect();
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
        let resources = OfferResources {
            slots,
            signaling_channel,
            data_channels,
        };
        drop(peers);
        self.retire_observation_tracks();
        Ok(resources)
    }

    fn initialize_sdp(
        &self,
        generation: Generation,
        capabilities: &agent_core::BootstrapCapabilities,
    ) -> Result<Result<agent_core::client_sdp::Bindings, agent_core::client_sdp::Error>, String>
    {
        let mut peers = self.peers.borrow_mut();
        let peer = peers
            .get_mut(&generation.get())
            .ok_or_else(|| "bootstrap generation is retired".to_owned())?;
        if peer.sdp.is_some() {
            return Err("bootstrap context is already installed".into());
        }
        let offer = peer
            .connection
            .local_description()
            .ok_or_else(|| "applied local bootstrap is absent".to_owned())?;
        let answer = peer
            .connection
            .remote_description()
            .ok_or_else(|| "applied remote bootstrap is absent".to_owned())?;
        let mut context = capabilities
            .applied_context(&offer.sdp(), &answer.sdp(), generation)
            .map_err(|_| "unsupported applied bootstrap SDP".to_owned())?;
        let preparation = (|| {
            let slots = peer
                .transceivers
                .iter()
                .map(|(slot, transceiver)| {
                    transceiver
                        .mid()
                        .map(|mid| (slot.clone(), mid))
                        .ok_or(agent_core::client_sdp::Error::Identifier)
                })
                .collect::<Result<Vec<_>, _>>()?;
            agent_core::adopt_bootstrap_media(&mut context, &slots)
        })();
        peer.sdp = Some(agent_core::client_sdp::Executor::new(context));
        Ok(preparation)
    }

    fn apply_answer(
        self: &Rc<Self>,
        generation: Generation,
        answer: String,
        capabilities: agent_core::BootstrapCapabilities,
    ) {
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
                    Ok(_) => match inner.initialize_sdp(generation, &capabilities) {
                        Ok(preparation) => {
                            inner.send_event(HostEvent::Rtc(RtcEvent::BootstrapMediaPrepared {
                                generation,
                                preparation,
                            }));
                            inner
                                .send_event(HostEvent::Rtc(RtcEvent::AnswerApplied { generation }));
                        }
                        Err(error) => {
                            inner.report_error(format!(
                                "generation={} bootstrap rejected: {error}",
                                generation.get()
                            ));
                            inner.send_event(HostEvent::Rtc(RtcEvent::BootstrapRejected {
                                generation,
                                message: error,
                            }));
                        }
                    },
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
        self.send_event(HostEvent::Rtc(RtcEvent::Failed {
            generation,
            message,
        }));
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
                self.logger.debug(
                    LOG_TARGET,
                    format!(
                        "starting browser HTTP request operation={} generation={:?} method={:?}",
                        operation.get(),
                        generation.map(Generation::get),
                        request.method,
                    ),
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
        self.logger.warn(LOG_TARGET, &message);
    }

    fn stop_local_sources(&self) {
        for (_, state) in std::mem::take(&mut *self.local_tracks.borrow_mut()) {
            state.track.stop();
        }
        self.publication_slots.borrow_mut().clear();
    }

    fn shutdown(&self) {
        self.closing.set(true);
        if self.closed.replace(true) {
            return;
        }
        self.close_publication_observers();
        self.close_observations();
        self.stop_local_sources();
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
        self.logger
            .info(LOG_TARGET, "browser agent runtime aborted");
    }
}

impl BrowserRuntime {
    fn admit_owned_data(
        &self,
        owner: u64,
        payload: Vec<u8>,
        options: Option<agent_core::ffi::DataSendOptions>,
    ) -> Result<
        futures_channel::oneshot::Receiver<Result<(), crate::engine::DataSendError>>,
        agent_core::ffi::ErrorInfo,
    > {
        use agent_core::ffi::{ErrorCode, ErrorInfo};
        self.inner
            .ensure_open()
            .map_err(|_| ErrorInfo::new(ErrorCode::Closed, "Data runtime is closed"))?;
        self.inner
            .actor
            .borrow()
            .as_ref()
            .ok_or_else(|| ErrorInfo::new(ErrorCode::NotReady, "Data runtime is not ready"))?
            .send_owned_data(owner, payload, options)
    }

    #[cfg(feature = "uniffi")]
    pub(crate) async fn next_exact_data(
        &self,
        owner: u64,
        observer: &agent_core::observer::DataSubscriptionStateObserver,
    ) -> Result<Option<agent_core::DataEvent>, agent_core::ffi::ErrorInfo> {
        use agent_core::ffi::{ErrorCode, ErrorInfo};
        use std::{collections::hash_map::Entry, task::Poll};
        struct Pending<'a>(&'a RuntimeInner, u64);
        impl Drop for Pending<'_> {
            fn drop(&mut self) {
                self.0.data_reads.borrow_mut().remove(&self.1);
            }
        }
        match self.inner.data_reads.borrow_mut().entry(owner) {
            Entry::Vacant(entry) => {
                entry.insert(None);
            }
            Entry::Occupied(_) => {
                return Err(ErrorInfo::new(
                    ErrorCode::Conflict,
                    "A data read is already pending",
                ));
            }
        }
        let _pending = Pending(&self.inner, owner);
        std::future::poll_fn(|cx| {
            let state = observer.state();
            if let Some(error) = state.terminal_error {
                return Poll::Ready(Err(error));
            }
            if state.closed || self.inner.is_closed() {
                return Poll::Ready(Ok(None));
            }
            match self
                .inner
                .intent_actor()
                .and_then(|actor| actor.read_owned_data(owner))
            {
                Ok(None) => {
                    *self
                        .inner
                        .data_reads
                        .borrow_mut()
                        .get_mut(&owner)
                        .expect("reserved read") = Some(cx.waker().clone());
                    Poll::Pending
                }
                result => Poll::Ready(result),
            }
        })
        .await
    }

    #[cfg(feature = "uniffi")]
    pub(crate) async fn send_exact_data(
        &self,
        owner: u64,
        payload: Vec<u8>,
        options: Option<agent_core::ffi::DataSendOptions>,
    ) -> Result<(), agent_core::ffi::ErrorInfo> {
        complete_data_send(self.admit_owned_data(owner, payload, options)?).await
    }
}

async fn complete_data_send(
    completion: futures_channel::oneshot::Receiver<Result<(), crate::engine::DataSendError>>,
) -> Result<(), agent_core::ffi::ErrorInfo> {
    completion
        .await
        .map_err(|_| {
            agent_core::ffi::ErrorInfo::new(
                agent_core::ffi::ErrorCode::Closed,
                "Data send was closed",
            )
        })?
        .map_err(|error| {
            use crate::engine::DataSendError;
            use agent_core::ffi::{ErrorCode, ErrorInfo};
            match error {
                DataSendError::Admission(error) => ErrorInfo::from(&error),
                DataSendError::Closed => {
                    ErrorInfo::new(ErrorCode::Closed, "Data runtime is closed")
                }
                DataSendError::Delivery(failure) => ErrorInfo::from(&failure),
            }
        })
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

async fn apply_sender_state(
    logger: &BrowserLogger,
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
        logger.warn(
            LOG_TARGET,
            &format!("browser rejected sender scalability mode: {error}"),
        );
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
        set_property(&value, "active", &JsValue::from_bool(config.active))?;
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
    for name in ["location", "content-type", "etag"] {
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

fn error_info_value(info: &agent_core::ffi::ErrorInfo) -> JsValue {
    let value = Object::new();
    set(&value, "code", info.code as u32);
    set(&value, "message", info.message.clone());
    value.into()
}

fn operation_error_value(info: agent_core::ffi::ErrorInfo) -> JsValue {
    let error = js_sys::Error::new(&info.message);
    let _ = Reflect::set(
        error.as_ref(),
        &JsValue::from_str("pulsebeamInfo"),
        &error_info_value(&info),
    );
    error.into()
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
