use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};

use agent_core::{
    AgentCommand, AgentConfig, ChannelId, ConnectionState, DataChannelBinding, DataChannelEffect,
    DataChannelEvent, DataChannelReliability, DataChannelSpec, DesiredState, Effect, Generation,
    HostEvent, HttpEffect, HttpEvent, HttpHeader, HttpMethod, HttpResponse, MediaSlot,
    MediaTopology, Notification, OfferResources, OperationId, RetryPolicy, RtcEffect, RtcEvent,
    SlotBinding, TimerEffect, TimerEvent, TopicMessage, TopicMode, TopicNotification,
    TopicPublisher, TopicRegistrations, TopicSend, TopicSubscriber,
};
use js_sys::{Array, Function, Object, Reflect, Uint8Array};
use serde::Deserialize;
use wasm_bindgen::{JsCast, JsValue, closure::Closure, prelude::wasm_bindgen};
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{
    AbortController, Event, Headers, MessageEvent, Request, RequestInit, Response, RtcBundlePolicy,
    RtcConfiguration, RtcDataChannel, RtcDataChannelInit, RtcDataChannelType, RtcPeerConnection,
    RtcPeerConnectionState, RtcRtpTransceiver, RtcRtpTransceiverDirection, RtcRtpTransceiverInit,
    RtcSdpType, RtcSessionDescriptionInit,
};

use crate::engine::{Driver, Input, SerialQueue, Turn};

const SIGNALING_LABEL: &str = "v1/sys/signaling";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RuntimeConfig {
    endpoint: String,
    room_id: String,
    #[serde(default)]
    request_headers: BTreeMap<String, String>,
    topology: TopologyConfig,
    #[serde(default)]
    topics: Vec<TopicConfig>,
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
struct TopicConfig {
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
    channels: BTreeMap<u64, DataChannel>,
    _state: Closure<dyn FnMut(Event)>,
}

impl Peer {
    fn close(self) {
        self.connection.set_onconnectionstatechange(None);
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

struct RuntimeInner {
    driver: RefCell<Driver>,
    queue: RefCell<SerialQueue<Input>>,
    desired: RefCell<DesiredState>,
    next_revision: Cell<u64>,
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
        let topics = topic_registrations(&config.topics);
        let request_headers = config
            .request_headers
            .into_iter()
            .map(|(name, value)| HttpHeader { name, value })
            .collect();
        let core_config = AgentConfig {
            endpoint: config.endpoint,
            room_id: config.room_id,
            request_headers,
            topology: MediaTopology {
                local_video: config.topology.local_video,
                local_audio: config.topology.local_audio,
                remote_video: config.topology.remote_video,
                remote_audio: config.topology.remote_audio,
            },
            manual_subscriptions: true,
            retry: RetryPolicy::default(),
        };
        let driver = Driver::new(core_config).map_err(|error| js_error(error.to_string()))?;
        let desired = DesiredState {
            topics,
            ..DesiredState::default()
        };
        Ok(Self {
            inner: Rc::new(RuntimeInner {
                driver: RefCell::new(driver),
                queue: RefCell::new(SerialQueue::default()),
                desired: RefCell::new(desired),
                next_revision: Cell::new(1),
                peers: RefCell::new(BTreeMap::new()),
                requests: RefCell::new(BTreeMap::new()),
                timers: RefCell::new(BTreeMap::new()),
                snapshot_listener: RefCell::new(None),
                event_listener: RefCell::new(None),
                error_listener: RefCell::new(None),
                last_error: RefCell::new(None),
                closed: Cell::new(false),
            }),
        })
    }

    pub fn set_snapshot_listener(&self, listener: Option<Function>) {
        *self.inner.snapshot_listener.borrow_mut() = listener;
        let listener = self.inner.snapshot_listener.borrow().clone();
        if let Some(listener) = listener {
            let snapshot = snapshot_value(self.inner.driver.borrow().snapshot());
            call_listener(&listener, &snapshot, "snapshot");
        }
    }

    pub fn set_event_listener(&self, listener: Option<Function>) {
        *self.inner.event_listener.borrow_mut() = listener;
    }

    pub fn set_error_listener(&self, listener: Option<Function>) {
        *self.inner.error_listener.borrow_mut() = listener;
    }

    pub fn connect(&self) -> Result<(), JsValue> {
        self.inner.ensure_open()?;
        self.inner.replace_connection_desired(true);
        Ok(())
    }

    pub fn force_reconnect(&self) -> Result<(), JsValue> {
        self.inner.ensure_open()?;
        let generation = self.inner.driver.borrow().snapshot().generation;
        let Some(generation) = generation else {
            return Err(js_error("cannot reconnect before a transport exists"));
        };
        self.inner
            .enqueue(Input::Event(HostEvent::Rtc(RtcEvent::Disconnected {
                generation,
            })));
        Ok(())
    }

    pub fn send_topic(&self, name: &str, mode: &str, payload: &[u8]) -> Result<(), JsValue> {
        self.inner.ensure_open()?;
        let mode = parse_topic_mode(mode)?;
        self.inner
            .enqueue(Input::Command(AgentCommand::SendTopic(TopicSend {
                publisher: TopicPublisher {
                    topic: name.to_owned(),
                    mode,
                },
                payload: payload.to_vec(),
            })));
        Ok(())
    }

    pub fn close(&self) {
        if self.inner.closed.get() {
            return;
        }
        self.inner.replace_connection_desired(false);
    }

    pub fn abort(&self) {
        self.inner.abort();
    }

    pub fn snapshot(&self) -> JsValue {
        snapshot_value(self.inner.driver.borrow().snapshot())
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
        self.inner.abort();
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

    fn replace_connection_desired(self: &Rc<Self>, connected: bool) {
        let mut desired = self.desired.borrow().clone();
        if desired.connected == connected && desired.revision != 0 {
            return;
        }
        desired.connected = connected;
        desired.revision = self.next_revision.get();
        self.next_revision
            .set(self.next_revision.get().saturating_add(1));
        *self.desired.borrow_mut() = desired.clone();
        self.enqueue(Input::Command(AgentCommand::ReplaceDesired(desired)));
    }

    fn enqueue(self: &Rc<Self>, input: Input) {
        if self.closed.get() {
            return;
        }
        if !self.queue.borrow_mut().push(input) {
            return;
        }

        loop {
            let Some(input) = self.queue.borrow_mut().pop() else {
                break;
            };
            let turn = self.driver.borrow_mut().turn(input);
            self.publish_turn(&turn);
            for effect in turn.effects {
                self.execute(effect);
            }
        }
        self.queue.borrow_mut().finish();
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
                    self.create_offer(generation);
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
                self.enqueue(Input::Event(HostEvent::Rtc(RtcEvent::Closed {
                    generation,
                })));
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
                    inner.enqueue(Input::Event(HostEvent::Rtc(RtcEvent::Connected {
                        generation,
                    })));
                }
                RtcPeerConnectionState::Disconnected | RtcPeerConnectionState::Failed => {
                    inner.enqueue(Input::Event(HostEvent::Rtc(RtcEvent::Disconnected {
                        generation,
                    })));
                }
                RtcPeerConnectionState::New
                | RtcPeerConnectionState::Connecting
                | RtcPeerConnectionState::Closed
                | _ => {}
            }
        }) as Box<dyn FnMut(Event)>);
        connection.set_onconnectionstatechange(Some(state.as_ref().unchecked_ref()));

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
            channels,
            _state: state,
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
                inner.enqueue(Input::Event(HostEvent::DataChannel(
                    DataChannelEvent::Opened {
                        generation,
                        channel: channel_id,
                    },
                )));
            }
        }) as Box<dyn FnMut(Event)>);
        channel.set_onopen(Some(open.as_ref().unchecked_ref()));

        let close_weak = Rc::downgrade(self);
        let close = Closure::wrap(Box::new(move |_event: Event| {
            if let Some(inner) = close_weak.upgrade() {
                inner.enqueue(Input::Event(HostEvent::DataChannel(
                    DataChannelEvent::Closed {
                        generation,
                        channel: channel_id,
                    },
                )));
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
                inner.enqueue(Input::Event(HostEvent::DataChannel(
                    DataChannelEvent::Message {
                        generation,
                        channel: channel_id,
                        payload,
                    },
                )));
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
                            inner.enqueue(Input::Event(HostEvent::Rtc(RtcEvent::OfferCreated {
                                generation,
                                offer,
                                resources,
                            })));
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
                    Ok(_) => inner.enqueue(Input::Event(HostEvent::Rtc(RtcEvent::AnswerApplied {
                        generation,
                    }))),
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
        self.enqueue(Input::Event(HostEvent::Rtc(RtcEvent::Failed {
            generation,
            message,
        })));
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
                            inner.enqueue(Input::Event(HostEvent::Http(HttpEvent::Response {
                                operation,
                                response,
                            })));
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
        self.enqueue(Input::Event(HostEvent::Http(HttpEvent::Failed {
            operation,
            message,
        })));
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
                        inner.enqueue(Input::Event(HostEvent::Timer(TimerEvent::Fired { timer })));
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
        self.enqueue(Input::Event(HostEvent::DataChannel(event)));
    }

    fn report_error(&self, message: String) {
        log::warn!("{message}");
        *self.last_error.borrow_mut() = Some(message.clone());
        let listener = self.error_listener.borrow().clone();
        if let Some(listener) = listener {
            call_listener(&listener, &JsValue::from_str(&message), "error");
        }
    }

    fn abort(&self) {
        if self.closed.replace(true) {
            return;
        }
        self.queue.borrow_mut().clear();
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

fn topic_registrations(topics: &[TopicConfig]) -> TopicRegistrations {
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
    set(&value, "participants", snapshot.participants.len());
    set(&value, "publications", snapshot.publications.len());
    set(&value, "videoBindings", snapshot.video.len());
    set(&value, "audioBindings", snapshot.audio.len());
    set(
        &value,
        "terminalFailure",
        snapshot
            .terminal_failure
            .as_ref()
            .map(|failure| failure.message.clone()),
    );
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
        Notification::Failure(failure) => {
            set(&value, "type", "failure");
            set(&value, "message", &failure.message);
            set(&value, "class", format!("{:?}", failure.class));
        }
        _ => {
            set(&value, "type", "state-change");
            set(&value, "detail", format!("{notification:?}"));
        }
    }
    value.into()
}

fn connection_name(state: &ConnectionState) -> String {
    match state {
        ConnectionState::RetryWaiting { attempt, .. } => format!("retry-waiting:{attempt}"),
        state => format!("{state:?}").to_lowercase(),
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
