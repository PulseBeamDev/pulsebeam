use std::collections::{BTreeMap, VecDeque};

use pulsebeam_agent_core::{CoreEffect, TransportGeneration};

#[cfg(target_arch = "wasm32")]
use crate::interop::EncodingConfig;
use crate::interop::{
    BrowserEvent, DataChannelConfig, GenerationEvent, PeerConfig, SenderPreset, WebError,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SenderUpdateQueue {
    desired: BTreeMap<String, SenderPreset>,
}

impl SenderUpdateQueue {
    pub fn new() -> Self {
        Self {
            desired: BTreeMap::new(),
        }
    }

    pub fn enqueue(&mut self, sender: impl Into<String>, preset: SenderPreset) {
        self.desired.insert(sender.into(), preset);
    }

    pub fn take_next(&mut self) -> Option<(String, SenderPreset)> {
        let key = self.desired.keys().next().cloned()?;
        self.desired.remove_entry(&key)
    }

    pub fn is_empty(&self) -> bool {
        self.desired.is_empty()
    }
}

impl Default for SenderUpdateQueue {
    fn default() -> Self {
        Self::new()
    }
}

pub struct WebTransport {
    config: PeerConfig,
    generation: Option<TransportGeneration>,
    channels: BTreeMap<String, DataChannelConfig>,
    sender_updates: SenderUpdateQueue,
    events: VecDeque<GenerationEvent<BrowserEvent>>,
    #[cfg(not(target_arch = "wasm32"))]
    sent: VecDeque<(TransportGeneration, String, Vec<u8>)>,
    #[cfg(target_arch = "wasm32")]
    browser: BrowserTransport,
}

impl WebTransport {
    pub fn new(config: PeerConfig) -> Result<Self, WebError> {
        let config = config.bounded();
        Ok(Self {
            config,
            generation: None,
            channels: BTreeMap::new(),
            sender_updates: SenderUpdateQueue::new(),
            events: VecDeque::new(),
            #[cfg(not(target_arch = "wasm32"))]
            sent: VecDeque::new(),
            #[cfg(target_arch = "wasm32")]
            browser: BrowserTransport::new(),
        })
    }

    pub fn config(&self) -> &PeerConfig {
        &self.config
    }

    pub fn generation(&self) -> Option<TransportGeneration> {
        self.generation
    }

    pub fn register_channel(&mut self, config: DataChannelConfig) {
        debug_assert!(!config.label.is_empty());
        self.channels.insert(config.label.clone(), config);
    }

    pub fn channels(&self) -> impl Iterator<Item = &DataChannelConfig> {
        self.channels.values()
    }

    pub async fn execute(&mut self, effect: CoreEffect) -> Result<(), WebError> {
        match effect {
            CoreEffect::Connect { generation } => self.connect(generation),
            CoreEffect::Send {
                generation,
                channel,
                payload,
                ..
            } => self.send(generation, channel.as_str(), payload),
        }
    }

    pub fn connect(&mut self, generation: TransportGeneration) -> Result<(), WebError> {
        if self.generation.is_some_and(|current| generation <= current) {
            return Err(WebError::StaleGeneration {
                expected: self.generation.unwrap_or(generation),
                received: generation,
            });
        }
        self.generation = Some(generation);
        #[cfg(target_arch = "wasm32")]
        self.browser
            .replace(&self.config, generation, &self.channels)?;
        Ok(())
    }

    pub fn send(
        &mut self,
        generation: TransportGeneration,
        channel: &str,
        payload: Vec<u8>,
    ) -> Result<(), WebError> {
        self.require_generation(generation)?;
        if !self.channels.contains_key(channel) {
            return Err(WebError::Browser(format!("unknown data channel {channel}")));
        }
        #[cfg(not(target_arch = "wasm32"))]
        self.sent
            .push_back((generation, channel.to_owned(), payload));
        #[cfg(target_arch = "wasm32")]
        self.browser.send(channel, &payload)?;
        Ok(())
    }

    pub fn queue_sender_update(&mut self, sender: impl Into<String>, preset: SenderPreset) {
        self.sender_updates.enqueue(sender, preset);
    }

    pub async fn flush_sender_updates(&mut self) -> Result<(), WebError> {
        while let Some((sender, preset)) = self.sender_updates.take_next() {
            #[cfg(target_arch = "wasm32")]
            self.browser.apply_sender_preset(&sender, &preset).await?;
            #[cfg(not(target_arch = "wasm32"))]
            let _ = (sender, preset);
        }
        Ok(())
    }

    pub fn poll_event(&mut self) -> Option<GenerationEvent<BrowserEvent>> {
        #[cfg(target_arch = "wasm32")]
        self.browser.drain_events(&mut self.events);
        self.events.pop_front()
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn poll_sent(&mut self) -> Option<(TransportGeneration, String, Vec<u8>)> {
        self.sent.pop_front()
    }

    pub fn inject_event(&mut self, event: GenerationEvent<BrowserEvent>) {
        self.events.push_back(event);
    }

    pub fn schedule_timer(
        &mut self,
        generation: TransportGeneration,
        delay: std::time::Duration,
    ) -> Result<(), WebError> {
        self.require_generation(generation)?;
        #[cfg(target_arch = "wasm32")]
        self.browser.schedule_timer(generation, delay)?;
        #[cfg(not(target_arch = "wasm32"))]
        let _ = delay;
        #[cfg(not(target_arch = "wasm32"))]
        self.events
            .push_back(GenerationEvent::new(generation, BrowserEvent::Timer));
        Ok(())
    }

    pub async fn create_offer(
        &mut self,
        generation: TransportGeneration,
    ) -> Result<String, WebError> {
        self.require_generation(generation)?;
        #[cfg(target_arch = "wasm32")]
        return self.browser.create_offer().await;
        #[cfg(not(target_arch = "wasm32"))]
        Ok("mock-offer".to_owned())
    }

    pub async fn set_answer(
        &mut self,
        generation: TransportGeneration,
        sdp: &str,
    ) -> Result<(), WebError> {
        self.require_generation(generation)?;
        #[cfg(target_arch = "wasm32")]
        {
            return self.browser.set_answer(sdp).await;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            debug_assert!(!sdp.is_empty());
            Ok(())
        }
    }

    fn require_generation(&self, generation: TransportGeneration) -> Result<(), WebError> {
        let Some(expected) = self.generation else {
            return Err(WebError::Browser("transport is not connected".to_owned()));
        };
        if expected != generation {
            debug_assert_ne!(expected, generation);
            return Err(WebError::StaleGeneration {
                expected,
                received: generation,
            });
        }
        Ok(())
    }

    pub async fn replace_sender_track(
        &mut self,
        sender: &str,
        track: Option<&MediaStreamTrackHandle>,
    ) -> Result<(), WebError> {
        #[cfg(target_arch = "wasm32")]
        {
            self.browser.replace_sender_track(sender, track).await
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let _ = (sender, track);
            Ok(())
        }
    }
}

#[derive(Clone)]
pub struct MediaStreamTrackHandle {
    #[cfg(target_arch = "wasm32")]
    raw: wasm_bindgen::JsValue,
    #[cfg(not(target_arch = "wasm32"))]
    id: String,
    #[cfg(not(target_arch = "wasm32"))]
    kind: String,
}

impl MediaStreamTrackHandle {
    #[cfg(target_arch = "wasm32")]
    pub fn from_js(raw: wasm_bindgen::JsValue) -> Self {
        Self { raw }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn mock(id: impl Into<String>, kind: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind: kind.into(),
        }
    }

    pub fn id(&self) -> String {
        #[cfg(target_arch = "wasm32")]
        {
            return property(&self.raw, "id")
                .ok()
                .and_then(|value| value.as_string())
                .unwrap_or_default();
        }
        #[cfg(not(target_arch = "wasm32"))]
        self.id.clone()
    }

    pub fn kind(&self) -> String {
        #[cfg(target_arch = "wasm32")]
        {
            return property(&self.raw, "kind")
                .ok()
                .and_then(|value| value.as_string())
                .unwrap_or_default();
        }
        #[cfg(not(target_arch = "wasm32"))]
        self.kind.clone()
    }

    pub fn set_enabled(&self, enabled: bool) -> Result<(), WebError> {
        #[cfg(target_arch = "wasm32")]
        {
            js_sys::Reflect::set(
                &self.raw,
                &wasm_bindgen::JsValue::from_str("enabled"),
                &enabled.into(),
            )
            .map_err(js_error)?;
        }
        #[cfg(not(target_arch = "wasm32"))]
        let _ = enabled;
        Ok(())
    }
}

pub struct MediaStreamHandle {
    #[cfg(target_arch = "wasm32")]
    raw: wasm_bindgen::JsValue,
    #[cfg(not(target_arch = "wasm32"))]
    tracks: Vec<MediaStreamTrackHandle>,
}

impl MediaStreamHandle {
    #[cfg(target_arch = "wasm32")]
    pub fn new() -> Result<Self, WebError> {
        let constructor = js_sys::Reflect::get(
            &js_sys::global(),
            &wasm_bindgen::JsValue::from_str("MediaStream"),
        )
        .map_err(js_error)?
        .dyn_into::<js_sys::Function>()
        .map_err(|_| WebError::Browser("MediaStream is unavailable".to_owned()))?;
        Ok(Self {
            raw: js_sys::Reflect::construct(&constructor, &js_sys::Array::new())
                .map_err(js_error)?,
        })
    }

    #[cfg(target_arch = "wasm32")]
    pub fn from_js(raw: wasm_bindgen::JsValue) -> Self {
        Self { raw }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn new() -> Self {
        Self { tracks: Vec::new() }
    }

    pub fn add_track(&mut self, track: &MediaStreamTrackHandle) -> Result<(), WebError> {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = call_method(&self.raw, "addTrack", std::slice::from_ref(&track.raw))?;
        }
        #[cfg(not(target_arch = "wasm32"))]
        self.tracks.push(track.clone());
        Ok(())
    }

    pub fn remove_track(&mut self, track: &MediaStreamTrackHandle) -> Result<(), WebError> {
        #[cfg(target_arch = "wasm32")]
        {
            let _ = call_method(&self.raw, "removeTrack", std::slice::from_ref(&track.raw))?;
        }
        #[cfg(not(target_arch = "wasm32"))]
        self.tracks.retain(|candidate| candidate.id() != track.id());
        Ok(())
    }

    pub fn tracks(&self) -> Result<Vec<MediaStreamTrackHandle>, WebError> {
        #[cfg(target_arch = "wasm32")]
        {
            let tracks = call_method(&self.raw, "getTracks", &[])?;
            let tracks = js_sys::Array::from(&tracks);
            return Ok(tracks.iter().map(MediaStreamTrackHandle::from_js).collect());
        }
        #[cfg(not(target_arch = "wasm32"))]
        Ok(self.tracks.clone())
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Default for MediaStreamHandle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_arch = "wasm32")]
struct BrowserTransport {
    peer: Option<wasm_bindgen::JsValue>,
    senders: BTreeMap<String, wasm_bindgen::JsValue>,
    channels: BTreeMap<String, wasm_bindgen::JsValue>,
    event_queue: std::rc::Rc<std::cell::RefCell<VecDeque<GenerationEvent<BrowserEvent>>>>,
    callbacks: Vec<wasm_bindgen::closure::Closure<dyn FnMut(wasm_bindgen::JsValue)>>,
}

#[cfg(target_arch = "wasm32")]
impl BrowserTransport {
    fn new() -> Self {
        Self {
            peer: None,
            senders: BTreeMap::new(),
            channels: BTreeMap::new(),
            event_queue: std::rc::Rc::new(std::cell::RefCell::new(VecDeque::new())),
            callbacks: Vec::new(),
        }
    }

    fn replace(
        &mut self,
        config: &PeerConfig,
        generation: TransportGeneration,
        channels: &BTreeMap<String, DataChannelConfig>,
    ) -> Result<(), WebError> {
        use js_sys::{Array, Function, Object, Reflect};
        use wasm_bindgen::{JsCast, JsValue, closure::Closure};

        self.close_peer();
        let options = Object::new();
        Reflect::set(&options, &JsValue::from_str("iceServers"), &Array::new())
            .map_err(js_error)?;
        Reflect::set(
            &options,
            &JsValue::from_str("bundlePolicy"),
            &JsValue::from_str(config.bundle_policy),
        )
        .map_err(js_error)?;
        Reflect::set(
            &options,
            &JsValue::from_str("rtcpMuxPolicy"),
            &JsValue::from_str(config.rtcp_mux_policy),
        )
        .map_err(js_error)?;
        Reflect::set(
            &options,
            &JsValue::from_str("iceCandidatePoolSize"),
            &0.into(),
        )
        .map_err(js_error)?;
        let constructor = Reflect::get(&js_sys::global(), &JsValue::from_str("RTCPeerConnection"))
            .map_err(js_error)?
            .dyn_into::<Function>()
            .map_err(|_| WebError::Browser("RTCPeerConnection is unavailable".to_owned()))?;
        let peer = Reflect::construct(&constructor, &Array::of1(&options)).map_err(js_error)?;

        let queue = self.event_queue.clone();
        let state_queue = queue.clone();
        let state_callback = Closure::wrap(Box::new(move |event: JsValue| {
            let state = Reflect::get(&event, &JsValue::from_str("target"))
                .ok()
                .and_then(|target| {
                    Reflect::get(&target, &JsValue::from_str("connectionState")).ok()
                })
                .and_then(|value| value.as_string());
            let browser_event = match state.as_deref() {
                Some("connected") => BrowserEvent::Connected,
                Some("failed") => BrowserEvent::Failed("peer connection failed".to_owned()),
                Some("closed") => BrowserEvent::Closed,
                _ => return,
            };
            state_queue
                .borrow_mut()
                .push_back(GenerationEvent::new(generation, browser_event));
        }) as Box<dyn FnMut(JsValue)>);
        Reflect::set(
            &peer,
            &JsValue::from_str("onconnectionstatechange"),
            state_callback.as_ref(),
        )
        .map_err(js_error)?;
        self.callbacks.push(state_callback);

        for channel in channels.values() {
            let raw = call_method(
                &peer,
                "createDataChannel",
                &[
                    JsValue::from_str(&channel.label),
                    data_channel_options(channel)?,
                ],
            )?;
            let callback =
                install_data_callback(&raw, generation, channel.label.clone(), queue.clone())?;
            self.callbacks.push(callback);
            self.channels.insert(channel.label.clone(), raw);
        }
        self.add_transceivers(&peer, config)?;
        self.peer = Some(peer);
        Ok(())
    }

    fn drain_events(&self, events: &mut VecDeque<GenerationEvent<BrowserEvent>>) {
        while let Some(event) = self.event_queue.borrow_mut().pop_front() {
            events.push_back(event);
        }
    }

    fn add_transceivers(
        &mut self,
        peer: &wasm_bindgen::JsValue,
        config: &PeerConfig,
    ) -> Result<(), WebError> {
        for (name, kind) in [
            ("main-audio", "audio"),
            ("main-video", "video"),
            ("aux-audio", "audio"),
            ("aux-video", "video"),
        ] {
            let direction = "sendonly";
            let init = transceiver_options(direction, kind == "video");
            let transceiver = call_method(
                peer,
                "addTransceiver",
                &[wasm_bindgen::JsValue::from_str(kind), init],
            )?;
            let sender = property(&transceiver, "sender")?;
            self.senders.insert(name.to_owned(), sender);
        }
        for _ in 0..config.audio_slots {
            let _ = call_method(
                peer,
                "addTransceiver",
                &[
                    wasm_bindgen::JsValue::from_str("audio"),
                    transceiver_options("recvonly", false),
                ],
            )?;
        }
        for _ in 0..config.video_slots {
            let _ = call_method(
                peer,
                "addTransceiver",
                &[
                    wasm_bindgen::JsValue::from_str("video"),
                    transceiver_options("recvonly", false),
                ],
            )?;
        }
        Ok(())
    }

    fn send(&self, channel: &str, payload: &[u8]) -> Result<(), WebError> {
        let raw = self
            .channels
            .get(channel)
            .ok_or_else(|| WebError::Browser(format!("channel {channel} is not open")))?;
        let bytes = js_sys::Uint8Array::from(payload);
        let _ = call_method(raw, "send", &[bytes.into()])?;
        Ok(())
    }

    async fn create_offer(&self) -> Result<String, WebError> {
        let peer = self
            .peer
            .as_ref()
            .ok_or_else(|| WebError::Browser("peer is not connected".to_owned()))?;
        let offer = await_promise(call_method(peer, "createOffer", &[])?).await?;
        let sdp = property(&offer, "sdp")?
            .as_string()
            .ok_or_else(|| WebError::Browser("offer has no SDP".to_owned()))?;
        let _ = await_promise(call_method(peer, "setLocalDescription", &[offer])?).await?;
        Ok(sdp)
    }

    async fn set_answer(&self, sdp: &str) -> Result<(), WebError> {
        let peer = self
            .peer
            .as_ref()
            .ok_or_else(|| WebError::Browser("peer is not connected".to_owned()))?;
        let answer = js_sys::Object::new();
        js_sys::Reflect::set(
            &answer,
            &wasm_bindgen::JsValue::from_str("type"),
            &wasm_bindgen::JsValue::from_str("answer"),
        )
        .map_err(js_error)?;
        js_sys::Reflect::set(
            &answer,
            &wasm_bindgen::JsValue::from_str("sdp"),
            &wasm_bindgen::JsValue::from_str(sdp),
        )
        .map_err(js_error)?;
        let _ = await_promise(call_method(peer, "setRemoteDescription", &[answer.into()])?).await?;
        Ok(())
    }

    async fn apply_sender_preset(
        &self,
        sender_name: &str,
        preset: &SenderPreset,
    ) -> Result<(), WebError> {
        let sender = self
            .senders
            .get(sender_name)
            .ok_or_else(|| WebError::Browser(format!("unknown sender {sender_name}")))?;
        let parameters = call_method(sender, "getParameters", &[])?;
        let encodings = property(&parameters, "encodings")?;
        for (index, encoding) in preset.video.iter().enumerate() {
            let raw = js_sys::Reflect::get(&encodings, &index.into()).map_err(js_error)?;
            if raw.is_undefined() {
                continue;
            }
            set_encoding(&raw, encoding)?;
        }
        let _ = await_promise(call_method(sender, "setParameters", &[parameters])?).await?;
        Ok(())
    }

    async fn replace_sender_track(
        &self,
        sender_name: &str,
        track: Option<&MediaStreamTrackHandle>,
    ) -> Result<(), WebError> {
        let sender = self
            .senders
            .get(sender_name)
            .ok_or_else(|| WebError::Browser(format!("unknown sender {sender_name}")))?;
        let track = track.map_or(wasm_bindgen::JsValue::NULL, |track| track.raw.clone());
        let _ = await_promise(call_method(sender, "replaceTrack", &[track])?).await?;
        Ok(())
    }

    fn schedule_timer(
        &self,
        generation: TransportGeneration,
        delay: std::time::Duration,
    ) -> Result<(), WebError> {
        use wasm_bindgen::{JsCast, closure::Closure};
        let window =
            web_sys::window().ok_or_else(|| WebError::Browser("window unavailable".to_owned()))?;
        let queue = self.event_queue.clone();
        let callback = Closure::once_into_js(move || {
            queue
                .borrow_mut()
                .push_back(GenerationEvent::new(generation, BrowserEvent::Timer));
        });
        window
            .set_timeout_with_callback_and_timeout_and_arguments_0(
                callback.as_ref().unchecked_ref(),
                crate::interop::duration_millis(delay),
            )
            .map_err(js_error)?;
        Ok(())
    }

    fn close_peer(&mut self) {
        if let Some(peer) = self.peer.take() {
            let _ = call_method(&peer, "close", &[]);
        }
        self.senders.clear();
        self.channels.clear();
        self.callbacks.clear();
    }
}

#[cfg(target_arch = "wasm32")]
fn data_channel_options(config: &DataChannelConfig) -> Result<wasm_bindgen::JsValue, WebError> {
    let object = js_sys::Object::new();
    js_sys::Reflect::set(
        &object,
        &wasm_bindgen::JsValue::from_str("ordered"),
        &config.ordered.into(),
    )
    .map_err(js_error)?;
    if let Some(max_retransmits) = config.max_retransmits {
        js_sys::Reflect::set(
            &object,
            &wasm_bindgen::JsValue::from_str("maxRetransmits"),
            &max_retransmits.into(),
        )
        .map_err(js_error)?;
    }
    Ok(object.into())
}

#[cfg(target_arch = "wasm32")]
fn transceiver_options(direction: &str, video: bool) -> wasm_bindgen::JsValue {
    let object = js_sys::Object::new();
    let _ = js_sys::Reflect::set(
        &object,
        &wasm_bindgen::JsValue::from_str("direction"),
        &wasm_bindgen::JsValue::from_str(direction),
    );
    if video {
        let encodings = js_sys::Array::new();
        for rid in ["q", "h", "f"] {
            let encoding = js_sys::Object::new();
            let _ = js_sys::Reflect::set(
                &encoding,
                &wasm_bindgen::JsValue::from_str("rid"),
                &wasm_bindgen::JsValue::from_str(rid),
            );
            let _ = js_sys::Reflect::set(
                &encoding,
                &wasm_bindgen::JsValue::from_str("active"),
                &false.into(),
            );
            let _ = js_sys::Reflect::set(
                &encoding,
                &wasm_bindgen::JsValue::from_str("scalabilityMode"),
                &wasm_bindgen::JsValue::from_str(crate::interop::DEFAULT_SCALABILITY_MODE),
            );
            encodings.push(&encoding);
        }
        let _ = js_sys::Reflect::set(
            &object,
            &wasm_bindgen::JsValue::from_str("sendEncodings"),
            &encodings,
        );
    }
    object.into()
}

#[cfg(target_arch = "wasm32")]
fn install_data_callback(
    raw: &wasm_bindgen::JsValue,
    generation: TransportGeneration,
    label: String,
    queue: std::rc::Rc<std::cell::RefCell<VecDeque<GenerationEvent<BrowserEvent>>>>,
) -> Result<wasm_bindgen::closure::Closure<dyn FnMut(wasm_bindgen::JsValue)>, WebError> {
    use wasm_bindgen::{JsValue, closure::Closure};
    let callback = Closure::wrap(Box::new(move |event: JsValue| {
        let data =
            js_sys::Reflect::get(&event, &JsValue::from_str("data")).unwrap_or(JsValue::UNDEFINED);
        if data.is_undefined() {
            return;
        }
        let bytes = js_sys::Uint8Array::new(&data).to_vec();
        queue.borrow_mut().push_back(GenerationEvent::new(
            generation,
            BrowserEvent::Data {
                label: label.clone(),
                payload: bytes,
            },
        ));
    }) as Box<dyn FnMut(JsValue)>);
    js_sys::Reflect::set(raw, &JsValue::from_str("onmessage"), callback.as_ref())
        .map_err(js_error)?;
    Ok(callback)
}

#[cfg(target_arch = "wasm32")]
fn call_method(
    target: &wasm_bindgen::JsValue,
    name: &str,
    arguments: &[wasm_bindgen::JsValue],
) -> Result<wasm_bindgen::JsValue, WebError> {
    let function = js_sys::Reflect::get(target, &wasm_bindgen::JsValue::from_str(name))
        .map_err(js_error)?
        .dyn_into::<js_sys::Function>()
        .map_err(|_| WebError::Browser(format!("{name} is not callable")))?;
    let args = js_sys::Array::new();
    for argument in arguments {
        args.push(argument);
    }
    function.apply(target, &args).map_err(js_error)
}

#[cfg(target_arch = "wasm32")]
fn property(target: &wasm_bindgen::JsValue, name: &str) -> Result<wasm_bindgen::JsValue, WebError> {
    js_sys::Reflect::get(target, &wasm_bindgen::JsValue::from_str(name)).map_err(js_error)
}

#[cfg(target_arch = "wasm32")]
async fn await_promise(value: wasm_bindgen::JsValue) -> Result<wasm_bindgen::JsValue, WebError> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;
    JsFuture::from(
        value
            .dyn_into::<js_sys::Promise>()
            .map_err(|_| WebError::Browser("expected Promise".to_owned()))?,
    )
    .await
    .map_err(js_error)
}

#[cfg(target_arch = "wasm32")]
fn set_encoding(raw: &wasm_bindgen::JsValue, encoding: &EncodingConfig) -> Result<(), WebError> {
    use wasm_bindgen::JsValue;
    let set = |name: &str, value: JsValue| {
        js_sys::Reflect::set(raw, &JsValue::from_str(name), &value).map_err(js_error)
    };
    set("rid", JsValue::from_str(&encoding.rid))?;
    set("active", encoding.active.into())?;
    if let Some(value) = encoding.scale_resolution_down_by {
        set("scaleResolutionDownBy", value.into())?;
    }
    if let Some(value) = encoding.max_bitrate_bps {
        set("maxBitrate", value.into())?;
    }
    if let Some(value) = encoding.max_framerate {
        set("maxFramerate", value.into())?;
    }
    set(
        "scalabilityMode",
        JsValue::from_str(&encoding.scalability_mode),
    )?;
    Ok(())
}

#[cfg(target_arch = "wasm32")]
fn js_error(error: wasm_bindgen::JsValue) -> WebError {
    WebError::Browser(
        error
            .as_string()
            .unwrap_or_else(|| "browser API failure".to_owned()),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::interop::BrowserEvent;

    #[test]
    fn reconnect_replaces_generation_and_rejects_stale_send() {
        let mut transport = WebTransport::new(PeerConfig::default()).unwrap();
        transport.register_channel(DataChannelConfig::reliable("v1/sys/signaling"));
        transport.connect(TransportGeneration::new(1)).unwrap();
        transport.connect(TransportGeneration::new(2)).unwrap();
        let result = transport.send(TransportGeneration::new(1), "v1/sys/signaling", vec![1]);
        assert!(matches!(result, Err(WebError::StaleGeneration { .. })));
        transport.inject_event(GenerationEvent::new(
            TransportGeneration::new(1),
            BrowserEvent::Connected,
        ));
        assert_eq!(
            transport.poll_event().unwrap().generation,
            TransportGeneration::new(1)
        );
    }
}
