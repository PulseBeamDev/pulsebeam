use std::panic::AssertUnwindSafe;

use anyhow::{Result, anyhow};
use futures_lite::FutureExt;
use reqwest::Client as HttpClient;
use str0m::{
    Event, Input, Output, Rtc, RtcError,
    change::SdpAnswer,
    media::{Direction, Simulcast, SimulcastLayer},
    net::{Protocol, Receive},
};
use tokio::sync::{broadcast, mpsc};
use tokio::{
    net::{TcpSocket, UdpSocket},
    time::Instant,
};

#[derive(Debug)]
pub enum AgentEvent {
    MediaAdded,
    StateChanged(str0m::IceConnectionState),
    Connected,
    Disconnected(DisconnectReason),
}

pub struct AgentBuilder {
    api_base: Option<String>,
    room_id: Option<String>,
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
    ) -> Result<Self> {
        let (event_tx, event_rx) = mpsc::channel(128);
        let mut rtc = Rtc::builder().clear_codecs().enable_h264(true).build();

        // TODO: allow configurable transceivers
        let mut sdp = rtc.sdp_api();
        let simulcast = Simulcast {
            send: vec![
                SimulcastLayer::new("f"),
                SimulcastLayer::new("h"),
                SimulcastLayer::new("q"),
            ],
            recv: vec![],
        };
        sdp.add_media(
            str0m::media::MediaKind::Video,
            Direction::SendOnly,
            None,
            None,
            Some(simulcast),
        );

        sdp.add_media(
            str0m::media::MediaKind::Video,
            Direction::RecvOnly,
            None,
            None,
            None,
        );

        let (offer, pending_offer) = sdp.apply().unwrap();
        let sdp_offer = offer.to_sdp_string();

        let uri = format!("{}/api/v1/rooms/{}", api_base, room_id);
        tracing::info!("connecting to {}", uri);
        let res = http_client
            .post(uri)
            .header("Content-Type", "application/sdp")
            .body(sdp_offer)
            .send()
            .await?;

        if !res.status().is_success() {
            return Err(anyhow!("join failed: {}", res.status()));
        }

        let location = res
            .headers()
            .get("Location")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("No Location header for cleanup"))?;

        let answer = res.text().await?;
        let answer = SdpAnswer::from_sdp_string(&answer)?;
        let sdp = rtc.sdp_api();
        sdp.accept_answer(pending_offer, answer)?;

        let actor = AgentActor {
            rtc,
            udp: socket,
            dropped_events: 0,
            event_tx,
        };

        let actor_task = AssertUnwindSafe(actor.run()).catch_unwind();
        tokio::spawn(actor_task);

        let _ = HttpClient::new().delete(&location).send().await;

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
    #[error("Unsupported media direction (must be SendOnly or RecvOnly)")]
    InvalidMediaDirection,
}

struct AgentActor {
    rtc: str0m::Rtc,
    udp: UdpSocket,
    event_tx: mpsc::Sender<AgentEvent>,
    dropped_events: u64,
}

impl AgentActor {
    async fn run(mut self) {
        let mut buf = vec![0u8; 2000];
        let mut maybe_deadline = self.poll();

        while let Some(deadline) = maybe_deadline {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => {
                    self.rtc.handle_input(Input::Timeout(Instant::now().into()));
                    maybe_deadline = self.poll();
                }
            }
        }
    }

    fn poll(&mut self) -> Option<Instant> {
        while self.rtc.is_alive() {
            match self.rtc.poll_output() {
                Ok(Output::Timeout(deadline)) => {
                    return Some(deadline.into());
                }
                Ok(Output::Transmit(tx)) => match tx.proto {
                    Protocol::Udp => {
                        self.udp.try_send_to(&tx.contents, tx.destination);
                    }
                    _ => {}
                },
                Ok(Output::Event(event)) => self.handle_event(event),
                Err(e) => {
                    self.disconnect(e.into());
                    return None;
                }
            }
        }

        None
    }

    fn handle_event(&mut self, event: str0m::Event) {}

    fn disconnect(&mut self, reason: DisconnectReason) {
        self.rtc.disconnect();
        self.emit_event(AgentEvent::Disconnected(reason));
    }

    fn emit_event(&mut self, event: AgentEvent) {
        match self.event_tx.try_send(event) {
            Err(mpsc::error::TrySendError::Full(event))
            | Err(mpsc::error::TrySendError::Closed(event)) => {
                self.dropped_events += 1;
                tracing::warn!("dropped event: {:?}", event);
            }
            _ => {}
        }
    }
}
