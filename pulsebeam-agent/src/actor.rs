use bytes::Bytes;
use futures_lite::FutureExt;
use reqwest::Client as HttpClient;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use str0m::{
    Event, Input, Output, Rtc, RtcError,
    change::SdpAnswer,
    error::SdpError,
    media::{Direction, MediaData, MediaKind, MediaTime, Mid, Pt, Simulcast, SimulcastLayer},
    net::{Protocol, Receive},
};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::Instant;

pub struct MediaFrame {
    pub ts: MediaTime,
    pub data: Bytes,
}

impl From<MediaData> for MediaFrame {
    fn from(value: MediaData) -> Self {
        Self {
            ts: value.time,
            data: value.data.into(),
        }
    }
}

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

fn new_media_channel(mid: Mid, pt: Pt) -> (MediaSender, MediaReceiver) {
    let (tx, rx) = mpsc::channel(64);
    let sender = MediaSender { mid, tx };
    let receiver = MediaReceiver { mid, rx, pt };
    (sender, receiver)
}

pub struct Agent {
    events: mpsc::Receiver<AgentEvent>,
}

impl Agent {
    pub async fn join(
        http_client: HttpClient,
        socket: UdpSocket,
        api_base: &str,
        room_id: &str,
    ) -> AgentResult<Self> {
        let (event_tx, event_rx) = mpsc::channel(128);

        let mut rtc = Rtc::builder().clear_codecs().enable_h264(true).build();

        let mut sdp = rtc.sdp_api();
        let simulcast = Simulcast {
            send: vec![
                SimulcastLayer::new("f"),
                SimulcastLayer::new("h"),
                SimulcastLayer::new("q"),
            ],
            recv: vec![],
        };

        // Create the SendOnly transceiver
        sdp.add_media(
            MediaKind::Video,
            Direction::SendOnly,
            None,
            None,
            Some(simulcast),
        );

        // Create the RecvOnly transceiver
        sdp.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);

        let (offer, pending_offer) = sdp.apply().unwrap();
        let sdp_offer = offer.to_sdp_string();

        let uri = format!("{}/api/v1/rooms/{}", api_base, room_id);
        let res = http_client
            .post(uri)
            .header("Content-Type", "application/sdp")
            .body(sdp_offer)
            .send()
            .await?;

        if !res.status().is_success() {
            return Err(AgentError::JoinStatus(res.status()));
        }

        let location = res
            .headers()
            .get("Location")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .ok_or(AgentError::MissingLocation)?;

        let body = res.text().await?;
        let answer = SdpAnswer::from_sdp_string(&body).map_err(|e| {
            tracing::error!("Failed to parse SDP answer. Raw body: {}", body);
            AgentError::Sdp(e)
        })?;
        rtc.sdp_api().accept_answer(pending_offer, answer)?;

        let actor = AgentActor {
            rtc,
            udp: socket,
            event_tx,
            receivers: HashMap::new(),
            dropped_events: 0,
        };

        let actor_task = AssertUnwindSafe(actor.run(location)).catch_unwind();
        tokio::spawn(actor_task);

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
    udp: UdpSocket,
    event_tx: mpsc::Sender<AgentEvent>,
    receivers: HashMap<Mid, MediaSender>,
    dropped_events: u64,
}

impl AgentActor {
    async fn run(mut self, location: String) {
        let mut buf = vec![0u8; 2000];
        let mut maybe_deadline = self.poll();

        while let Some(deadline) = maybe_deadline {
            tokio::select! {
                // Some(envelope) = self.sender.recv() => {
                //     if let Some(writer) = self.rtc.writer(envelope.mid) {
                //         let pt = writer.payload_params().nth(0).unwrap().pt();
                //         let now = Instant::now();
                //         let frame = envelope.frame;
                //         if let Err(e) = writer.write(pt, now.into(), frame.ts, frame.data) {
                //             tracing::error!("Writer error for mid {}: {:?}", envelope.mid, e);
                //         }
                //     }
                //     maybe_deadline = self.poll();
                // }
                res = self.udp.recv_from(&mut buf) => {
                    if let Ok((n, source)) = res {
                        let Ok(buf) = (&buf[..n]).try_into() else {
                            continue;
                        };

                        let input = Input::Receive(
                            Instant::now().into(),
                            Receive {
                                proto: Protocol::Udp,
                                source,
                                destination: self.udp.local_addr().unwrap(),
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
                    // Using try_send_to to maintain non-blocking poll
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
            Event::MediaData(data) => {
                if let Some(tx) = self.receivers.get(&data.mid) {
                    tx.try_send(data.into());
                }
            }
            _ => {}
        }
    }

    fn disconnect(&mut self, reason: DisconnectReason) {
        self.rtc.disconnect();
        self.emit_event(AgentEvent::Disconnected(reason));
    }

    fn emit_event(&mut self, event: AgentEvent) {
        if self.event_tx.try_send(event).is_err() {
            self.dropped_events += 1;
        }
    }
}
