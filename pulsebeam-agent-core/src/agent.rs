use alloc::{collections::VecDeque, format, string::String, vec};
use core::time::Duration;

use crate::{
    AgentConfig, AgentError, AgentNotification, AgentSnapshot, ClientConnectionState, ClientState,
    ConnectionPhase, DataChannelEvent, EventDisposition, NegotiatedTopology, RtcEffect, StateError,
    context::{
        AgentContext, AgentEffect, AgentEvent, DataChannelConfig, HttpEvent, RtcEvent, TimerEvent,
    },
    http::{HttpHeader, HttpMethod, HttpRequest, HttpResponse},
    id::{DataChannelId, Generation, RequestId, TimerId},
};
use proto::prelude::Message;

const RETRY_BASE: Duration = Duration::from_millis(500);
const RETRY_MAX: Duration = Duration::from_secs(5);

pub struct Agent {
    cx: AgentContext,
    config: AgentConfig,
    snapshot: AgentSnapshot,
    desired: ClientState,
    notifications: VecDeque<AgentNotification>,
    session: Session,
    resource: Option<Resource>,
    retries: u8,
    negotiated: Option<NegotiatedTopology>,
    latency_fixed: bool,
}

#[derive(Clone, Copy)]
enum Session {
    Idle,
    Offer {
        generation: Generation,
        channel: DataChannelId,
        replace: bool,
    },
    Http {
        generation: Generation,
        channel: DataChannelId,
        replace: bool,
        request: RequestId,
    },
    Answer {
        generation: Generation,
        channel: DataChannelId,
    },
    Channel {
        generation: Generation,
        channel: DataChannelId,
    },
    Active {
        generation: Generation,
        channel: DataChannelId,
    },
    Retry {
        timer: TimerId,
    },
    Terminal,
}
struct Resource {
    uri: String,
    etag: String,
}

impl Agent {
    pub fn new(config: AgentConfig) -> Self {
        Self {
            cx: AgentContext::new(),
            config,
            snapshot: AgentSnapshot::default(),
            desired: ClientState::default(),
            notifications: VecDeque::new(),
            session: Session::Idle,
            resource: None,
            retries: 0,
            negotiated: None,
            latency_fixed: false,
        }
    }
    pub fn config(&self) -> &AgentConfig {
        &self.config
    }
    pub fn desired_state(&self) -> &ClientState {
        &self.desired
    }
    pub fn snapshot(&self) -> &AgentSnapshot {
        &self.snapshot
    }
    pub fn next_effect(&mut self) -> Option<AgentEffect> {
        self.cx.next_effect()
    }
    pub fn next_notification(&mut self) -> Option<AgentNotification> {
        self.notifications.pop_front()
    }
    pub fn set_state(&mut self, state: ClientState) -> Result<(), StateError> {
        state.validate(self.config.topology())?;
        if self.latency_fixed && state.latency == crate::LatencyIntent::Adaptive {
            return Err(StateError::LatencyCannotReturnAdaptive);
        }
        self.latency_fixed |= matches!(state.latency, crate::LatencyIntent::Fixed { .. });
        self.desired = state;
        self.reconcile();
        if let Session::Active {
            generation,
            channel,
        } = self.session
        {
            self.cx
                .emit(AgentEffect::Rtc(RtcEffect::ReconcileLocalSlots {
                    generation,
                    slots: self.desired.local_slots.clone(),
                }));
            self.emit_intent(generation, channel);
        }
        Ok(())
    }
    pub fn handle(&mut self, event: AgentEvent) -> EventDisposition {
        if !self.cx.accepts(&event) {
            return EventDisposition::IgnoredStale;
        }
        match event {
            AgentEvent::Rtc(RtcEvent::OfferCreated {
                generation,
                offer,
                topology,
            }) => {
                if let Session::Offer {
                    generation: current,
                    channel,
                    replace,
                } = self.session
                    && current == generation
                {
                    if topology.validate(self.config.topology()).is_err() {
                        self.terminal("invalid negotiated topology");
                    } else {
                        self.negotiated = Some(topology);
                        self.request(generation, channel, replace, offer);
                    }
                }
            }
            AgentEvent::Rtc(RtcEvent::AnswerApplied { generation }) => {
                if let Session::Answer {
                    generation: current,
                    channel,
                } = self.session
                    && current == generation
                {
                    self.session = Session::Channel {
                        generation,
                        channel,
                    };
                }
            }
            AgentEvent::Rtc(RtcEvent::Disconnected { .. }) => self.retry(),
            AgentEvent::Http(HttpEvent::Failed { id }) => {
                if matches!(self.session, Session::Http { request, .. } if request == id) {
                    self.cx.complete_request(id);
                    self.retry();
                }
            }
            AgentEvent::Http(HttpEvent::Response { id, response }) => self.response(id, response),
            AgentEvent::Timer(TimerEvent::Fired { id }) => {
                if matches!(self.session, Session::Retry { timer } if timer == id) {
                    self.start(self.resource.is_some());
                }
            }
            AgentEvent::DataChannel(DataChannelEvent::Opened { generation, id }) => {
                if let Session::Channel {
                    generation: current,
                    channel,
                } = self.session
                    && current == generation
                    && channel == id
                {
                    self.session = Session::Active {
                        generation,
                        channel,
                    };
                    self.retries = 0;
                    self.phase(ConnectionPhase::Connected);
                    self.emit_intent(generation, id);
                }
            }
            AgentEvent::DataChannel(DataChannelEvent::Message {
                generation,
                id,
                payload,
            }) => {
                if matches!(self.session, Session::Active { generation: current, channel } if current == generation && channel == id)
                {
                    self.apply_signal(&payload);
                }
            }
            _ => {}
        }
        EventDisposition::Accepted
    }

    fn apply_signal(&mut self, bytes: &[u8]) {
        let Ok(message) = proto::signaling::ServerMessage::decode(bytes) else {
            self.notifications
                .push_back(AgentNotification::Error(AgentError::Protocol(
                    String::from("invalid signaling message"),
                )));
            return;
        };
        match message.payload {
            Some(proto::signaling::server_message::Payload::Error(error)) => self
                .notifications
                .push_back(AgentNotification::Error(AgentError::Protocol(error))),
            Some(proto::signaling::server_message::Payload::State(state)) => {
                self.apply_server_state(state);
            }
            None => self
                .notifications
                .push_back(AgentNotification::Error(AgentError::Protocol(
                    String::from("empty signaling message"),
                ))),
        }
    }

    fn apply_server_state(&mut self, state: proto::signaling::ServerState) {
        if state.snapshot {
            self.snapshot.publications.clear();
            self.snapshot.video_bindings.clear();
            self.snapshot.audio_bindings.clear();
        }
        for id in state.publications_removed {
            if let Some(index) = self
                .snapshot
                .publications
                .iter()
                .position(|publication| publication.track_id == id)
            {
                self.snapshot.publications.remove(index);
                self.notifications
                    .push_back(AgentNotification::PublicationRemoved { track_id: id });
            }
        }
        for publication in state.publications_added {
            let kind = match proto::signaling::TrackKind::try_from(publication.kind) {
                Ok(proto::signaling::TrackKind::Audio) => crate::MediaKind::Audio,
                Ok(proto::signaling::TrackKind::Video) => crate::MediaKind::Video,
                _ => continue,
            };
            let current = crate::Publication {
                track_id: publication.track_id,
                participant_id: publication.participant_id,
                kind,
            };
            if !self
                .snapshot
                .publications
                .iter()
                .any(|item| item.track_id == current.track_id)
            {
                self.notifications
                    .push_back(AgentNotification::PublicationAdded(current.clone()));
                self.snapshot.publications.push(current);
            }
        }
        if let Some(video) = state.video {
            self.snapshot.video_bindings = video
                .items
                .into_iter()
                .map(|binding| crate::VideoBinding {
                    mid: binding.mid,
                    track_id: binding.track_id,
                    paused: binding.paused,
                })
                .collect();
            self.notifications
                .push_back(AgentNotification::VideoBindingsChanged);
        }
        if let Some(audio) = state.audio {
            self.snapshot.audio_bindings = audio
                .items
                .into_iter()
                .map(|binding| crate::AudioBinding {
                    mid: binding.mid,
                    track_id: binding.track_id,
                    level_dbov: binding.level_dbov,
                })
                .collect();
            self.notifications
                .push_back(AgentNotification::AudioBindingsChanged);
        }
    }

    fn emit_intent(&mut self, generation: Generation, channel: DataChannelId) {
        let Some(topology) = self.negotiated.as_ref() else {
            self.terminal("missing negotiated topology");
            return;
        };
        let video = self
            .desired
            .subscriptions
            .video
            .iter()
            .zip(&topology.video_receive_mids)
            .map(|(request, mid)| proto::signaling::VideoIntent {
                mid: mid.clone(),
                track_id: request.track_id.clone(),
                height: request.target_height,
                min_height: request.min_height,
                min_fps: request.min_fps,
                priority: request.priority,
            })
            .collect();
        let publish = topology
            .upstream_slots
            .iter()
            .filter_map(|slot| {
                self.desired
                    .local_slots
                    .iter()
                    .find(|wanted| wanted.slot == slot.slot)
                    .map(|wanted| (slot, wanted))
            })
            .flat_map(|(slot, wanted)| {
                [
                    proto::signaling::PublishIntent {
                        mid: slot.audio_mid.clone(),
                        active: wanted.audio.attached && !wanted.audio.muted,
                    },
                    proto::signaling::PublishIntent {
                        mid: slot.video_mid.clone(),
                        active: wanted.video.attached && !wanted.video.muted,
                    },
                ]
            })
            .collect();
        let ext = match self.desired.latency {
            crate::LatencyIntent::Adaptive => None,
            crate::LatencyIntent::Fixed { min_ms, max_ms } => Some(proto::signaling::Extensions {
                playout_delay: Some(proto::signaling::PlayoutDelay { min_ms, max_ms }),
            }),
        };
        let message = proto::signaling::ClientMessage {
            payload: Some(proto::signaling::client_message::Payload::Intent(
                proto::signaling::ClientIntent {
                    video,
                    audio: Some(proto::signaling::AudioIntent {
                        pinned: self.desired.subscriptions.audio.pinned.clone(),
                        auto: self.desired.subscriptions.audio.auto,
                    }),
                    publish,
                    ext,
                },
            )),
        };
        self.cx
            .emit(AgentEffect::DataChannel(crate::DataChannelEffect::Send {
                generation,
                id: channel,
                payload: message.encode_to_vec(),
            }));
    }
    fn reconcile(&mut self) {
        match (self.desired.connection, self.session) {
            (ClientConnectionState::Disconnected, Session::Idle | Session::Terminal) => {}
            (ClientConnectionState::Disconnected, _) => self.close(),
            (ClientConnectionState::Connected, Session::Idle | Session::Terminal) => {
                self.start(false);
            }
            _ => {}
        }
    }
    fn start(&mut self, replace: bool) {
        let (Some(generation), Some(channel)) = (self.cx.generation(), self.cx.data_channel_id())
        else {
            self.terminal("identifier space exhausted");
            return;
        };
        self.cx.emit(AgentEffect::Rtc(RtcEffect::CreateTransport {
            generation,
            topology: self.config.topology().clone(),
            signaling_channel: channel,
        }));
        self.cx.dc_open(
            generation,
            channel,
            DataChannelConfig::reliable(String::from("v1/sys/signaling")),
        );
        self.session = Session::Offer {
            generation,
            channel,
            replace,
        };
        self.phase(if replace {
            ConnectionPhase::Reconnecting
        } else {
            ConnectionPhase::Connecting
        });
    }
    fn request(
        &mut self,
        generation: Generation,
        channel: DataChannelId,
        replace: bool,
        offer: String,
    ) {
        let request = if replace {
            self.patch(offer)
        } else {
            self.create(offer)
        };
        let Some(request) = request else {
            self.terminal("missing identity or identifier space exhausted");
            return;
        };
        self.session = Session::Http {
            generation,
            channel,
            replace,
            request,
        };
    }
    fn create(&mut self, offer: String) -> Option<RequestId> {
        let identity = self.desired.identity.as_ref()?;
        let mut uri = format!(
            "{}/rooms/{}/participants?manual_sub=true",
            self.config.endpoint().trim_end_matches('/'),
            encode_query(&identity.room)
        );
        for entry in &identity.metadata {
            uri.push_str(&format!(
                "&metadata.{}={}",
                encode_query(&entry.name),
                encode_query(&entry.value)
            ));
        }
        let mut headers = vec![HttpHeader {
            name: String::from("Content-Type"),
            value: String::from("application/sdp"),
        }];
        if let Some(token) = &identity.token {
            headers.push(HttpHeader {
                name: String::from("Authorization"),
                value: format!("Bearer {token}"),
            });
        }
        self.cx.http_request(HttpRequest {
            method: HttpMethod::Post,
            uri,
            headers,
            body: offer.into_bytes(),
        })
    }
    fn patch(&mut self, offer: String) -> Option<RequestId> {
        let resource = self.resource.as_ref()?;
        self.cx.http_request(HttpRequest {
            method: HttpMethod::Patch,
            uri: resource.uri.clone(),
            headers: vec![
                HttpHeader {
                    name: String::from("Content-Type"),
                    value: String::from("application/sdp"),
                },
                HttpHeader {
                    name: String::from("If-Match"),
                    value: resource.etag.clone(),
                },
            ],
            body: offer.into_bytes(),
        })
    }
    fn response(&mut self, id: RequestId, response: HttpResponse) {
        let Session::Http {
            generation,
            channel,
            replace,
            request,
        } = self.session
        else {
            return;
        };
        if request != id {
            return;
        };
        self.cx.complete_request(id);
        if response.status == 404 && replace {
            self.resource = None;
            self.start(false);
            return;
        }
        if !(200..300).contains(&response.status) {
            if matches!(response.status, 400 | 401 | 403) {
                self.terminal("server rejected connection");
            } else {
                self.retry();
            }
            return;
        }
        let Some(uri) = response
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case("location"))
            .map(|h| h.value.clone())
        else {
            self.terminal("missing Location");
            return;
        };
        let Some(etag) = response
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case("etag"))
            .map(|h| h.value.clone())
        else {
            self.terminal("missing ETag");
            return;
        };
        let Ok(answer) = String::from_utf8(response.body) else {
            self.terminal("invalid answer");
            return;
        };
        self.snapshot.participant_id = response
            .headers
            .iter()
            .find(|header| header.name.eq_ignore_ascii_case("pb-participant-id"))
            .map(|header| header.value.clone())
            .or_else(|| uri.rsplit('/').next().map(String::from));
        self.resource = Some(Resource { uri, etag });
        self.cx.emit(AgentEffect::Rtc(RtcEffect::ApplyAnswer {
            generation,
            answer,
        }));
        self.session = Session::Answer {
            generation,
            channel,
        };
    }
    fn retry(&mut self) {
        if self.desired.connection == ClientConnectionState::Disconnected {
            self.close();
            return;
        }
        if let Some(generation) = generation_of(self.session) {
            self.cx.rtc_close(generation);
        }
        let factor = 1_u32
            .checked_shl(u32::from(self.retries.min(4)))
            .unwrap_or(16);
        let after = RETRY_BASE.saturating_mul(factor).min(RETRY_MAX);
        self.retries = self.retries.saturating_add(1);
        let Some(timer) = self.cx.schedule_timer(after) else {
            self.terminal("identifier space exhausted");
            return;
        };
        self.session = Session::Retry { timer };
        self.phase(ConnectionPhase::Reconnecting);
    }
    fn close(&mut self) {
        if let Some(resource) = self.resource.take() {
            let _ = self.cx.http_request(HttpRequest {
                method: HttpMethod::Delete,
                uri: resource.uri,
                headers: vec![],
                body: vec![],
            });
        }
        if let Some(generation) = generation_of(self.session) {
            self.cx.rtc_close(generation);
        }
        self.session = Session::Idle;
        self.phase(ConnectionPhase::Disconnected);
    }
    fn terminal(&mut self, reason: &str) {
        self.session = Session::Terminal;
        self.phase(ConnectionPhase::Failed);
        self.notifications
            .push_back(AgentNotification::Error(AgentError::Terminal(
                String::from(reason),
            )));
    }
    fn phase(&mut self, phase: ConnectionPhase) {
        if self.snapshot.connection != phase {
            self.snapshot.connection = phase.clone();
            self.notifications
                .push_back(AgentNotification::Connection(phase));
        }
    }
}

fn generation_of(session: Session) -> Option<Generation> {
    match session {
        Session::Offer { generation, .. }
        | Session::Http { generation, .. }
        | Session::Answer { generation, .. }
        | Session::Channel { generation, .. }
        | Session::Active { generation, .. } => Some(generation),
        Session::Idle | Session::Retry { .. } | Session::Terminal => None,
    }
}

fn encode_query(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::panic,
        reason = "tests use direct assertions"
    )]

    use alloc::{string::String, vec};

    use super::*;

    fn agent() -> Agent {
        let topology = crate::Topology::new(vec![], 0, 0).unwrap();
        Agent::new(AgentConfig::new("https://example.test/api/v1", topology).unwrap())
    }

    #[test]
    fn create_offer_answer_then_channel_reaches_active() {
        let mut agent = agent();
        agent
            .set_state(ClientState {
                connection: ClientConnectionState::Connected,
                identity: Some(crate::ConnectionIdentity {
                    room: String::from("room"),
                    token: None,
                    metadata: vec![],
                }),
                ..ClientState::default()
            })
            .unwrap();
        let AgentEffect::Rtc(RtcEffect::CreateTransport {
            generation,
            signaling_channel,
            ..
        }) = agent.next_effect().unwrap()
        else {
            panic!()
        };
        let _ = agent.next_effect();
        agent.handle(AgentEvent::Rtc(RtcEvent::OfferCreated {
            generation,
            offer: String::from("offer"),
            topology: crate::NegotiatedTopology {
                upstream_slots: vec![],
                video_receive_mids: vec![],
                audio_receive_mids: vec![],
            },
        }));
        let AgentEffect::Http(crate::HttpEffect::Request { id, request }) =
            agent.next_effect().unwrap()
        else {
            panic!()
        };
        assert_eq!(request.method, HttpMethod::Post);
        agent.handle(AgentEvent::Http(HttpEvent::Response {
            id,
            response: HttpResponse {
                status: 201,
                headers: vec![
                    HttpHeader {
                        name: String::from("Location"),
                        value: String::from("https://example.test/resource"),
                    },
                    HttpHeader {
                        name: String::from("ETag"),
                        value: String::from("tag"),
                    },
                ],
                body: String::from("answer").into_bytes(),
            },
        }));
        let _ = agent.next_effect();
        agent.handle(AgentEvent::Rtc(RtcEvent::AnswerApplied { generation }));
        agent.handle(AgentEvent::DataChannel(DataChannelEvent::Opened {
            generation,
            id: signaling_channel,
        }));
        assert_eq!(agent.snapshot().connection, ConnectionPhase::Connected);
    }

    #[test]
    fn fixed_latency_cannot_return_to_adaptive() {
        let mut agent = agent();
        agent
            .set_state(ClientState {
                latency: crate::LatencyIntent::Fixed {
                    min_ms: 10,
                    max_ms: 20,
                },
                ..ClientState::default()
            })
            .unwrap();

        assert_eq!(
            agent.set_state(ClientState::default()),
            Err(StateError::LatencyCannotReturnAdaptive)
        );
    }

    #[test]
    fn snapshot_replaces_publications_and_bindings() {
        let mut agent = agent();
        agent.apply_server_state(proto::signaling::ServerState {
            snapshot: true,
            participants_added: vec![],
            participants_removed: vec![],
            publications_added: vec![proto::signaling::Publication {
                track_id: String::from("video"),
                participant_id: String::from("participant"),
                kind: i32::from(proto::signaling::TrackKind::Video),
            }],
            publications_removed: vec![],
            video: Some(proto::signaling::VideoBindings {
                items: vec![proto::signaling::VideoBinding {
                    track_id: String::from("video"),
                    mid: String::from("7"),
                    paused: false,
                }],
            }),
            audio: None,
        });

        assert_eq!(agent.snapshot.publications.len(), 1);
        assert_eq!(agent.snapshot.video_bindings.len(), 1);
    }
}
