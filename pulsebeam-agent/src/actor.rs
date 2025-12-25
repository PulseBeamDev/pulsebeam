use anyhow::{Result, anyhow};
use futures_lite::FutureExt;
use reqwest::Client as HttpClient;
use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use str0m::{
    Event, Input, Output, Rtc, RtcError,
    change::SdpAnswer,
    media::{Direction, MediaData, MediaKind, Mid, Simulcast, SimulcastLayer},
    net::{Protocol, Receive},
};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::Instant;

#[derive(Debug)]
pub enum AgentEvent {
    /// Emitted when a RecvOnly transceiver is ready.
    /// The user pulls MediaData from the receiver handle.
    OnReceiver(Mid, mpsc::Receiver<MediaData>),
    StateChanged(str0m::IceConnectionState),
    Connected,
    Disconnected(DisconnectReason),
}

/// A handle to push media into a specific SendOnly transceiver.
pub struct MediaSender {
    mid: Mid,
    tx: mpsc::Sender<(Mid, MediaData)>,
}

impl MediaSender {
    pub async fn send(&self, sample: MediaData) -> Result<()> {
        self.tx
            .send((self.mid, sample))
            .await
            .map_err(|_| anyhow!("Actor closed"))
    }
}

pub struct Agent {
    events: mpsc::Receiver<AgentEvent>,
    video_sender: MediaSender,
}

impl Agent {
    pub async fn join(
        http_client: HttpClient,
        socket: UdpSocket,
        api_base: &str,
        room_id: &str,
    ) -> Result<Self> {
        let (event_tx, event_rx) = mpsc::channel(128);
        let (sample_tx, sample_rx) = mpsc::channel(1024);

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
        let video_mid = sdp.add_media(
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
            return Err(anyhow!("join failed: {}", res.status()));
        }

        let location = res
            .headers()
            .get("Location")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("No Location header"))?;

        let answer = SdpAnswer::from_sdp_string(&res.text().await?)?;
        rtc.sdp_api().accept_answer(pending_offer, answer)?;

        let actor = AgentActor {
            rtc,
            udp: socket,
            event_tx,
            sample_rx,
            receivers: HashMap::new(),
            dropped_events: 0,
        };

        let actor_task = AssertUnwindSafe(actor.run(location)).catch_unwind();
        tokio::spawn(actor_task);

        Ok(Self {
            events: event_rx,
            video_sender: MediaSender {
                mid: video_mid,
                tx: sample_tx,
            },
        })
    }

    pub async fn next_event(&mut self) -> Option<AgentEvent> {
        self.events.recv().await
    }

    pub fn video_sender(&self) -> &MediaSender {
        &self.video_sender
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
    sample_rx: mpsc::Receiver<(Mid, MediaData)>,
    receivers: HashMap<Mid, mpsc::Sender<MediaData>>,
    dropped_events: u64,
}

impl AgentActor {
    async fn run(mut self, location: String) {
        let mut buf = vec![0u8; 2000];
        let mut maybe_deadline = self.poll();

        while let Some(deadline) = maybe_deadline {
            tokio::select! {
                Some((mid, sample)) = self.sample_rx.recv() => {
                    if let Some(writer) = self.rtc.writer(mid) {
                        let pt = writer.payload_params().nth(0).unwrap().pt();
                        if let Err(e) = writer.write(pt, sample.network_time, sample.time, sample.data) {
                            tracing::error!("Writer error for mid {}: {:?}", mid, e);
                        }
                    }
                    maybe_deadline = self.poll();
                }
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
            Event::MediaAdded(media) if media.direction == Direction::RecvOnly => {
                let (tx, rx) = mpsc::channel(1024);
                self.receivers.insert(media.mid, tx);
                self.emit_event(AgentEvent::OnReceiver(media.mid, rx));
            }
            Event::MediaData(data) => {
                if let Some(tx) = self.receivers.get(&data.mid) {
                    if let Err(_) = tx.try_send(data) {
                        // User is not pulling data fast enough from the receiver transceiver
                    }
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
        if let Err(_) = self.event_tx.try_send(event) {
            self.dropped_events += 1;
        }
    }
}
