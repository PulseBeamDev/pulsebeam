use futures_lite::{FutureExt, Stream, StreamExt};
use reqwest::Client as HttpClient;
use std::panic::AssertUnwindSafe;
use std::{collections::HashMap, net::SocketAddr};
use str0m::media::MediaAdded;
use str0m::{
    Candidate, Event, Input, Output, Rtc, RtcError,
    change::SdpAnswer,
    error::SdpError,
    media::{Direction, MediaKind, Mid, Pt, Simulcast, SimulcastLayer},
    net::{Protocol, Receive},
};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_stream::StreamMap;

use crate::MediaFrame;

#[derive(Debug)]
pub enum AgentEvent {
    SenderAdded(MediaSender),
    ReceiverAdded(MediaReceiver),
    StateChanged(str0m::IceConnectionState),
    Connected,
    Disconnected(DisconnectReason),
}

#[derive(thiserror::Error, Debug)]
pub enum AgentError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("RTC engine error: {0}")]
    Rtc(#[from] str0m::RtcError),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Join failed with status: {0}")]
    JoinStatus(reqwest::StatusCode),

    #[error("Missing 'Location' header for resource cleanup")]
    MissingLocation,

    #[error("SDP negotiation failed: {0}")]
    SdpNegotiation(&'static str),

    #[error("SDP parsing error: {0}")]
    Sdp(SdpError),
}

// Update the result type for Agent::join
pub type AgentResult<T> = std::result::Result<T, AgentError>;

#[derive(Debug)]
pub struct MediaSender {
    mid: Mid,
    tx: mpsc::Sender<MediaFrame>,
}

impl MediaSender {
    pub fn try_send(&self, frame: MediaFrame) {
        if self.tx.try_send(frame).is_err() {}
    }
}

#[derive(Debug)]
pub struct MediaReceiver {
    pub mid: Mid,
    pt: Pt,
    rx: mpsc::Receiver<MediaFrame>,
}

impl MediaReceiver {
    pub async fn recv(&mut self) -> Option<MediaFrame> {
        self.rx.recv().await
    }
}

impl Stream for MediaReceiver {
    type Item = MediaFrame;
    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

fn new_media_channel(mid: Mid, pt: Pt) -> (MediaSender, MediaReceiver) {
    let (tx, rx) = mpsc::channel(64);
    let sender = MediaSender { mid, tx };
    let receiver = MediaReceiver { mid, rx, pt };
    (sender, receiver)
}

pub struct AgentBuilder {
    api_base: String,
    room_id: String,
    http_client: Option<HttpClient>,
    socket: Option<UdpSocket>,
}

impl AgentBuilder {
    pub fn new(api_base: impl Into<String>, room_id: impl Into<String>) -> Self {
        Self {
            api_base: api_base.into(),
            room_id: room_id.into(),
            http_client: None,
            socket: None,
        }
    }

    pub fn http_client(mut self, client: HttpClient) -> Self {
        self.http_client = Some(client);
        self
    }

    pub fn socket(mut self, socket: UdpSocket) -> Self {
        self.socket = Some(socket);
        self
    }

    pub async fn join(self) -> AgentResult<Agent> {
        let http_client = self.http_client.unwrap_or_default();
        let socket = match self.socket {
            Some(s) => s,
            None => UdpSocket::bind("0.0.0.0:0").await?,
        };

        Agent::join(http_client, socket, &self.api_base, &self.room_id).await
    }
}

pub struct Agent {
    events: mpsc::Receiver<AgentEvent>,
}

impl Agent {
    async fn join(
        http_client: HttpClient,
        socket: UdpSocket,
        api_base: &str,
        room_id: &str,
    ) -> AgentResult<Self> {
        let span = tracing::info_span!("join", room_id = %room_id);
        let _enter = span.enter();

        tracing::info!("Initiating join to room at {}", api_base);

        let (event_tx, event_rx) = mpsc::channel(128);

        let mut rtc = Rtc::builder().clear_codecs().enable_h264(true).build();
        let addr = format!("192.168.1.63:{}", socket.local_addr().unwrap().port())
            .parse()
            .unwrap();
        rtc.add_local_candidate(Candidate::host(addr, Protocol::Udp).unwrap());
        let mut sdp = rtc.sdp_api();

        let simulcast = Simulcast {
            send: vec![
                SimulcastLayer::new("f"),
                SimulcastLayer::new("h"),
                SimulcastLayer::new("q"),
            ],
            recv: vec![],
        };

        tracing::debug!("Configuring transceivers (Video: SendOnly w/ Simulcast, Video: RecvOnly)");
        let mut medias = vec![];

        // TODO: Major hack!
        let mid = sdp.add_media(
            MediaKind::Video,
            Direction::SendOnly,
            None,
            None,
            // Some(simulcast.clone()),
            None,
        );
        medias.push(MediaAdded {
            mid,
            kind: MediaKind::Video,
            direction: Direction::SendOnly,
            // simulcast: Some(simulcast.clone()),
            simulcast: None,
        });
        sdp.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);

        let (offer, pending_offer) = sdp.apply().unwrap();
        let sdp_offer = offer.to_sdp_string();
        tracing::trace!(sdp = %sdp_offer, "Generated local SDP offer");

        let uri = format!("{}/api/v1/rooms/{}", api_base, room_id);
        tracing::info!(uri, "Sending SDP offer to signaling server");

        let res = http_client
            .post(&uri)
            .header("Content-Type", "application/sdp")
            .body(sdp_offer)
            .send()
            .await
            .map_err(|e| {
                tracing::error!(error = ?e, uri, "Network error during HTTP signaling");
                e
            })?;

        let status = res.status();
        if !status.is_success() {
            tracing::error!(uri, status = %status, "Signaling server rejected the join request");
            return Err(AgentError::JoinStatus(status));
        }

        let location = res
            .headers()
            .get("Location")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                tracing::error!("Response status was 2xx but 'Location' header is missing");
                AgentError::MissingLocation
            })?;

        tracing::debug!(
            location,
            "Resource created successfully, tracking for cleanup"
        );

        let body = res.text().await?;
        tracing::trace!(sdp = %body, "Received SDP answer from signaling server");

        let answer = SdpAnswer::from_sdp_string(&body).map_err(|e| {
            // We log the raw body here because parsing errors are usually due to
            // specific lines in the SDP string that str0m doesn't like.
            tracing::error!(error = ?e, raw_body = %body, "Failed to parse remote SDP answer");
            AgentError::Sdp(e)
        })?;

        rtc.sdp_api()
            .accept_answer(pending_offer, answer)
            .map_err(|e| {
                tracing::error!(error = ?e, "RTC engine rejected the remote SDP answer");
                e
            })?;

        tracing::info!("SDP exchange successful. Initializing AgentActor background task.");

        let actor = AgentActor {
            rtc,
            addr,
            udp: socket,
            event_tx,
            senders: StreamMap::new(),
            receivers: HashMap::new(),
            dropped_events: 0,
        };

        let actor_task = AssertUnwindSafe(actor.run(medias, location)).catch_unwind();
        tokio::spawn(actor_task);

        tracing::info!("Agent joined and task spawned successfully");
        Ok(Self { events: event_rx })
    }

    pub async fn next_event(&mut self) -> Option<AgentEvent> {
        self.events.recv().await
    }
}

#[derive(thiserror::Error, Debug)]
pub enum DisconnectReason {
    #[error("RTC engine error")]
    RtcError(#[from] RtcError),
    #[error("ICE connection disconnected")]
    IceDisconnected,
}

struct AgentActor {
    rtc: str0m::Rtc,
    addr: SocketAddr,
    udp: UdpSocket,
    event_tx: mpsc::Sender<AgentEvent>,
    senders: StreamMap<Mid, MediaReceiver>,
    receivers: HashMap<Mid, MediaSender>,
    dropped_events: u64,
}

impl AgentActor {
    async fn run(mut self, mut medias: Vec<MediaAdded>, location: String) {
        let mut buf = vec![0u8; 2000];
        let mut maybe_deadline = self.poll();
        for media in medias.drain(..) {
            self.handle_media_added(media);
        }

        while let Some(deadline) = maybe_deadline {
            tokio::select! {
                Some((mid, frame)) = self.senders.next() => {
                    if let Some(writer) = self.rtc.writer(mid) {
                        let pt = writer.payload_params().nth(0).unwrap().pt();
                        let now = Instant::now();
                        if let Err(e) = writer.write(pt, now.into(), frame.ts, frame.data) {
                            tracing::error!("Writer error for mid {}: {:?}", mid, e);
                        }
                        tracing::info!("[{}] received from {}", frame.ts.numer(), mid);
                    }
                    maybe_deadline = self.poll();
                }
                res = self.udp.recv_from(&mut buf) => {
                    if let Ok((n, source)) = res {
                        let Ok(buf) = (&buf[..n]).try_into() else {
                            tracing::warn!("invalid rtc packet");
                            continue;
                        };

                        let input = Input::Receive(
                            Instant::now().into(),
                            Receive {
                                proto: Protocol::Udp,
                                source,
                                destination: self.addr,
                                contents: buf,
                            }
                        );
                        if let Err(e) = self.rtc.handle_input(input) {
                            self.disconnect(e.into());
                            break;
                        }
                        maybe_deadline = self.poll();
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    if let Err(e) = self.rtc.handle_input(Input::Timeout(Instant::now().into())) {
                        self.disconnect(e.into());
                        break;
                    }
                    maybe_deadline = self.poll();
                }
            }

            if !self.rtc.is_alive() {
                break;
            }
        }

        let _ = HttpClient::new().delete(&location).send().await;
    }

    fn poll(&mut self) -> Option<Instant> {
        while self.rtc.is_alive() {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(deadline)) => {
                    return Some(deadline.into());
                }
                Ok(Output::Transmit(tx)) => {
                    let _ = self.udp.try_send_to(&tx.contents, tx.destination);
                }
                Ok(Output::Event(event)) => self.handle_event(event),
                Err(e) => {
                    self.disconnect(e.into());
                    return None;
                }
            }
        }
        None
    }

    fn handle_event(&mut self, event: Event) {
        match event {
            Event::Connected => self.emit_event(AgentEvent::Connected),
            Event::IceConnectionStateChange(s) => self.emit_event(AgentEvent::StateChanged(s)),
            Event::MediaAdded(media) => {
                self.handle_media_added(media);
            }
            Event::MediaData(data) => {
                if let Some(tx) = self.receivers.get(&data.mid) {
                    tx.try_send(data.into());
                }
            }
            _ => {}
        }
    }

    fn handle_media_added(&mut self, media: MediaAdded) {
        // TODO: handle error
        let pt = self
            .rtc
            .media(media.mid)
            .unwrap()
            .remote_pts()
            .first()
            .unwrap();
        let (tx, rx) = new_media_channel(media.mid, *pt);

        match media.direction {
            Direction::SendOnly => {
                self.senders.insert(media.mid, rx);
                self.emit_event(AgentEvent::SenderAdded(tx));
            }
            Direction::RecvOnly => {
                self.receivers.insert(media.mid, tx);
                self.emit_event(AgentEvent::ReceiverAdded(rx));
            }
            Direction::SendRecv => {
                unreachable!("SendRecv is not supported");
            }
            Direction::Inactive => {
                unreachable!("Inactive is not supported");
            }
        }
    }

    fn disconnect(&mut self, reason: DisconnectReason) {
        self.rtc.disconnect();
        self.emit_event(AgentEvent::Disconnected(reason));
    }

    fn emit_event(&mut self, event: AgentEvent) {
        if self.event_tx.try_send(event).is_err() {
            self.dropped_events += 1;
            tracing::warn!("event dropped");
        }
    }
}
