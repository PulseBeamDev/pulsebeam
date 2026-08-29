use std::{
    cell::RefCell,
    collections::VecDeque,
    rc::{Rc, Weak},
};

use agent_core::{
    Agent, AgentConfig, AgentEffect, AgentNotification, AgentSnapshot, ClientState, StateError,
};

#[cfg(any(test, target_arch = "wasm32"))]
use agent_core::{AgentEvent, EventDisposition};

use crate::watch::{self, Sender, TryRecvError};

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::JsCast;

pub struct WebAgent {
    desired: Sender<ClientState>,
    inner: Rc<Inner>,
}

pub type WebEvent = AgentNotification;

pub struct WebAgentConfig {
    pub core: AgentConfig,
}

#[derive(thiserror::Error, Debug)]
pub enum WebAgentError {
    #[error("web actor has stopped")]
    Stopped,
    #[error(transparent)]
    State(#[from] StateError),
}

struct Inner {
    actor: RefCell<Actor>,
    runtime: RefCell<BrowserRuntime>,
}

struct Actor {
    core: Agent,
    desired: watch::Receiver<ClientState>,
    effects: VecDeque<AgentEffect>,
}

impl WebAgent {
    pub fn new(config: WebAgentConfig) -> Self {
        let (desired, receiver) = watch::channel::<ClientState>();
        let inner = Rc::new(Inner {
            actor: RefCell::new(Actor::new(config.core, receiver)),
            runtime: RefCell::new(BrowserRuntime::new()),
        });
        Self { desired, inner }
    }

    pub fn set_state(&self, state: ClientState) -> Result<(), WebAgentError> {
        self.desired
            .send(state)
            .map_err(|_| WebAgentError::Stopped)?;
        Inner::pump(&self.inner)
    }

    pub fn snapshot(&self) -> AgentSnapshot {
        let _ = Inner::pump(&self.inner);
        self.inner.actor.borrow().core.snapshot().clone()
    }

    pub fn next_event(&self) -> Option<WebEvent> {
        let _ = Inner::pump(&self.inner);
        self.inner.actor.borrow_mut().core.next_notification()
    }

    pub fn close(&self) -> Result<(), WebAgentError> {
        self.set_state(ClientState::default())
    }
}

impl Inner {
    fn pump(this: &Rc<Self>) -> Result<(), WebAgentError> {
        loop {
            let effect = {
                let mut actor = this.actor.borrow_mut();
                actor.apply_desired()?;
                actor.effects.pop_front()
            };
            let Some(effect) = effect else {
                return Ok(());
            };
            this.runtime
                .borrow_mut()
                .execute(effect, Rc::downgrade(this));
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn feed(this: &Weak<Self>, event: AgentEvent) {
        let Some(inner) = this.upgrade() else {
            return;
        };
        let _ = inner.actor.borrow_mut().feed(event);
        let _ = Self::pump(&inner);
    }
}

impl Actor {
    fn new(config: AgentConfig, desired: watch::Receiver<ClientState>) -> Self {
        Self {
            core: Agent::new(config),
            desired,
            effects: VecDeque::new(),
        }
    }

    fn apply_desired(&mut self) -> Result<(), WebAgentError> {
        match self.desired.try_recv() {
            Ok(state) => {
                self.core.set_state(state)?;
                self.drain_effects();
            }
            Err(TryRecvError::Empty | TryRecvError::Closed) => {}
        }
        Ok(())
    }

    #[cfg(any(test, target_arch = "wasm32"))]
    fn feed(&mut self, event: AgentEvent) -> EventDisposition {
        let disposition = self.core.handle(event);
        self.drain_effects();
        disposition
    }

    fn drain_effects(&mut self) {
        while let Some(effect) = self.core.next_effect() {
            self.effects.push_back(effect);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
struct BrowserRuntime;

#[cfg(not(target_arch = "wasm32"))]
impl BrowserRuntime {
    fn new() -> Self {
        Self
    }

    fn execute(&mut self, _: AgentEffect, _: Weak<Inner>) {}
}

#[cfg(target_arch = "wasm32")]
struct BrowserRuntime {
    transport: Option<Transport>,
    timers: std::collections::BTreeMap<agent_core::TimerId, gloo_timers::callback::Timeout>,
    requests: std::collections::BTreeMap<agent_core::RequestId, web_sys::AbortController>,
}

#[cfg(target_arch = "wasm32")]
struct Transport {
    generation: agent_core::Generation,
    peer: web_sys::RtcPeerConnection,
    channels: std::collections::BTreeMap<agent_core::DataChannelId, Channel>,
    _connection_callback: wasm_bindgen::closure::Closure<dyn FnMut(web_sys::Event)>,
    _topology: WebTopology,
}

#[cfg(target_arch = "wasm32")]
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

#[cfg(target_arch = "wasm32")]
struct Channel {
    channel: web_sys::RtcDataChannel,
    _open_callback: wasm_bindgen::closure::Closure<dyn FnMut(web_sys::Event)>,
    _close_callback: wasm_bindgen::closure::Closure<dyn FnMut(web_sys::Event)>,
    _message_callback: wasm_bindgen::closure::Closure<dyn FnMut(web_sys::MessageEvent)>,
}

#[cfg(target_arch = "wasm32")]
impl BrowserRuntime {
    fn new() -> Self {
        Self {
            transport: None,
            timers: std::collections::BTreeMap::new(),
            requests: std::collections::BTreeMap::new(),
        }
    }

    fn execute(&mut self, effect: AgentEffect, actor: Weak<Inner>) {
        match effect {
            AgentEffect::Rtc(effect) => self.rtc(effect, actor),
            AgentEffect::DataChannel(effect) => self.channel(effect, actor),
            AgentEffect::Timer(effect) => self.timer(effect, actor),
            AgentEffect::Http(effect) => self.http(effect, actor),
        }
    }

    fn rtc(&mut self, effect: agent_core::RtcEffect, actor: Weak<Inner>) {
        match effect {
            agent_core::RtcEffect::CreateTransport {
                generation,
                topology,
                ..
            } => self.create_transport(generation, topology, actor),
            agent_core::RtcEffect::ApplyAnswer { generation, answer } => {
                let Some(transport) = self
                    .transport
                    .as_ref()
                    .filter(|item| item.generation == generation)
                else {
                    return;
                };
                let peer = transport.peer.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let description =
                        web_sys::RtcSessionDescriptionInit::new(web_sys::RtcSdpType::Answer);
                    description.set_sdp(&answer);
                    if wasm_bindgen_futures::JsFuture::from(
                        peer.set_remote_description(&description),
                    )
                    .await
                    .is_ok()
                    {
                        Inner::feed(
                            &actor,
                            AgentEvent::Rtc(agent_core::RtcEvent::AnswerApplied { generation }),
                        );
                    } else {
                        Inner::feed(
                            &actor,
                            AgentEvent::Rtc(agent_core::RtcEvent::Disconnected { generation }),
                        );
                    }
                });
            }
            agent_core::RtcEffect::CloseTransport { generation } => {
                if self
                    .transport
                    .as_ref()
                    .is_some_and(|item| item.generation == generation)
                {
                    let transport = self.transport.take();
                    if let Some(transport) = transport {
                        transport.peer.close();
                    }
                    for controller in self.requests.values() {
                        controller.abort();
                    }
                    self.requests.clear();
                }
            }
            agent_core::RtcEffect::ReconcileLocalSlots { .. } => {}
        }
    }

    fn create_transport(
        &mut self,
        generation: agent_core::Generation,
        topology: agent_core::Topology,
        actor: Weak<Inner>,
    ) {
        for controller in self.requests.values() {
            controller.abort();
        }
        self.requests.clear();
        if let Some(previous) = self.transport.take() {
            previous.peer.close();
        }
        let Ok(peer) = web_sys::RtcPeerConnection::new() else {
            Inner::feed(
                &actor,
                AgentEvent::Rtc(agent_core::RtcEvent::Disconnected { generation }),
            );
            return;
        };
        let callback_peer = peer.clone();
        let callback_actor = actor.clone();
        let connection_callback = wasm_bindgen::closure::Closure::wrap(Box::new(move |_| {
            if matches!(
                callback_peer.connection_state(),
                web_sys::RtcPeerConnectionState::Disconnected
                    | web_sys::RtcPeerConnectionState::Failed
                    | web_sys::RtcPeerConnectionState::Closed
            ) {
                Inner::feed(
                    &callback_actor,
                    AgentEvent::Rtc(agent_core::RtcEvent::Disconnected { generation }),
                );
            }
        })
            as Box<dyn FnMut(web_sys::Event)>);
        peer.set_onconnectionstatechange(Some(connection_callback.as_ref().unchecked_ref()));
        let web_topology = WebTopology::create(&peer, &topology);
        let offer_peer = peer.clone();
        let offer_topology = web_topology.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let Ok(offer) = wasm_bindgen_futures::JsFuture::from(offer_peer.create_offer()).await
            else {
                Inner::feed(
                    &actor,
                    AgentEvent::Rtc(agent_core::RtcEvent::Disconnected { generation }),
                );
                return;
            };
            let offer: web_sys::RtcSessionDescriptionInit = offer.unchecked_into();
            if wasm_bindgen_futures::JsFuture::from(offer_peer.set_local_description(&offer))
                .await
                .is_err()
            {
                Inner::feed(
                    &actor,
                    AgentEvent::Rtc(agent_core::RtcEvent::Disconnected { generation }),
                );
                return;
            }
            let Some(sdp) = js_sys::Reflect::get(&offer, &wasm_bindgen::JsValue::from_str("sdp"))
                .ok()
                .and_then(|value| value.as_string())
            else {
                Inner::feed(
                    &actor,
                    AgentEvent::Rtc(agent_core::RtcEvent::Disconnected { generation }),
                );
                return;
            };
            let Some(topology) = offer_topology.negotiated() else {
                Inner::feed(
                    &actor,
                    AgentEvent::Rtc(agent_core::RtcEvent::Disconnected { generation }),
                );
                return;
            };
            Inner::feed(
                &actor,
                AgentEvent::Rtc(agent_core::RtcEvent::OfferCreated {
                    generation,
                    offer: sdp,
                    topology,
                }),
            );
        });
        self.transport = Some(Transport {
            generation,
            peer,
            channels: std::collections::BTreeMap::new(),
            _connection_callback: connection_callback,
            _topology: web_topology,
        });
    }

    fn channel(&mut self, effect: agent_core::DataChannelEffect, actor: Weak<Inner>) {
        match effect {
            agent_core::DataChannelEffect::Open {
                generation,
                id,
                config,
            } => {
                let Some(transport) = self
                    .transport
                    .as_mut()
                    .filter(|item| item.generation == generation)
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
                let channel = transport
                    .peer
                    .create_data_channel_with_data_channel_dict(&config.label, &options);
                let open_actor = actor.clone();
                let open_callback = wasm_bindgen::closure::Closure::wrap(Box::new(move |_| {
                    Inner::feed(
                        &open_actor,
                        AgentEvent::DataChannel(agent_core::DataChannelEvent::Opened {
                            generation,
                            id,
                        }),
                    );
                })
                    as Box<dyn FnMut(web_sys::Event)>);
                channel.set_onopen(Some(open_callback.as_ref().unchecked_ref()));
                let close_actor = actor.clone();
                let close_callback = wasm_bindgen::closure::Closure::wrap(Box::new(move |_| {
                    Inner::feed(
                        &close_actor,
                        AgentEvent::DataChannel(agent_core::DataChannelEvent::Closed {
                            generation,
                            id,
                        }),
                    );
                })
                    as Box<dyn FnMut(web_sys::Event)>);
                channel.set_onclose(Some(close_callback.as_ref().unchecked_ref()));
                let message_actor = actor.clone();
                let message_callback = wasm_bindgen::closure::Closure::wrap(Box::new(
                    move |event: web_sys::MessageEvent| {
                        let data = event.data();
                        if data.is_instance_of::<js_sys::ArrayBuffer>() {
                            let bytes = js_sys::Uint8Array::new(&data).to_vec();
                            Inner::feed(
                                &message_actor,
                                AgentEvent::DataChannel(agent_core::DataChannelEvent::Message {
                                    generation,
                                    id,
                                    payload: bytes,
                                }),
                            );
                        }
                    },
                )
                    as Box<dyn FnMut(web_sys::MessageEvent)>);
                channel.set_onmessage(Some(message_callback.as_ref().unchecked_ref()));
                transport.channels.insert(
                    id,
                    Channel {
                        channel,
                        _open_callback: open_callback,
                        _close_callback: close_callback,
                        _message_callback: message_callback,
                    },
                );
            }
            agent_core::DataChannelEffect::Close { generation, id } => {
                if let Some(transport) = self
                    .transport
                    .as_mut()
                    .filter(|item| item.generation == generation)
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
                    .filter(|item| item.generation == generation)
                    .and_then(|item| item.channels.get(&id))
                else {
                    return;
                };
                if channel.channel.send_with_u8_array(&payload).is_err() {
                    Inner::feed(
                        &actor,
                        AgentEvent::DataChannel(agent_core::DataChannelEvent::WriteFailed {
                            generation,
                            id,
                        }),
                    );
                }
            }
        }
    }

    fn timer(&mut self, effect: agent_core::TimerEffect, actor: Weak<Inner>) {
        match effect {
            agent_core::TimerEffect::Schedule { id, after } => {
                let millis = u32::try_from(after.as_millis()).unwrap_or(u32::MAX);
                let timeout = gloo_timers::callback::Timeout::new(millis, move || {
                    Inner::feed(
                        &actor,
                        AgentEvent::Timer(agent_core::TimerEvent::Fired { id }),
                    );
                });
                self.timers.insert(id, timeout);
            }
            agent_core::TimerEffect::Cancel { id } => {
                let _ = self.timers.remove(&id);
            }
        }
    }

    fn http(&mut self, effect: agent_core::HttpEffect, actor: Weak<Inner>) {
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
        let Ok(request) = builder.body(wasm_bindgen::JsValue::from(js_sys::Uint8Array::from(
            request.body.as_slice(),
        ))) else {
            Inner::feed(
                &actor,
                AgentEvent::Http(agent_core::HttpEvent::Failed { id }),
            );
            return;
        };
        if let Some(controller) = controller {
            self.requests.insert(id, controller);
        }
        wasm_bindgen_futures::spawn_local(async move {
            let response = request.send().await;
            let event = match response {
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
            Inner::feed(&actor, event);
        });
    }
}

#[cfg(target_arch = "wasm32")]
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

#[cfg(target_arch = "wasm32")]
fn transceiver(
    peer: &web_sys::RtcPeerConnection,
    kind: &str,
    direction: web_sys::RtcRtpTransceiverDirection,
) -> web_sys::RtcRtpTransceiver {
    let init = web_sys::RtcRtpTransceiverInit::new();
    init.set_direction(direction);
    peer.add_transceiver_with_str_and_init(kind, &init)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests use direct assertions")]

    use super::*;
    use agent_core::{ClientConnectionState, ConnectionIdentity, Topology};

    fn config() -> AgentConfig {
        AgentConfig::new(
            "https://example.test/api/v1",
            Topology::new(vec![], 0, 0).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn state_revisions_coalesce_before_actor_reconciliation() {
        let (sender, receiver) = watch::channel();
        let mut actor = Actor::new(config(), receiver);
        sender.send(ClientState::default()).unwrap();
        sender
            .send(ClientState {
                connection: ClientConnectionState::Connected,
                identity: Some(ConnectionIdentity {
                    room: String::from("room"),
                    token: None,
                    metadata: vec![],
                }),
                ..ClientState::default()
            })
            .unwrap();
        actor.apply_desired().unwrap();
        assert!(matches!(
            actor.effects.pop_front(),
            Some(AgentEffect::Rtc(_))
        ));
        assert!(matches!(
            actor.effects.pop_front(),
            Some(AgentEffect::DataChannel(_))
        ));
        assert!(actor.effects.is_empty());
    }

    #[test]
    fn stale_callbacks_are_rejected_before_effect_dispatch() {
        let (_, receiver) = watch::channel();
        let mut actor = Actor::new(config(), receiver);
        assert_eq!(
            actor.feed(AgentEvent::Rtc(agent_core::RtcEvent::Disconnected {
                generation: agent_core::Generation::new(99),
            })),
            EventDisposition::IgnoredStale
        );
        assert!(actor.effects.is_empty());
    }
}
