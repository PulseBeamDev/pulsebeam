use std::panic::AssertUnwindSafe;

use anyhow::{Result, anyhow};
use bytes::Bytes;
use futures_lite::FutureExt;
use reqwest::Client as HttpClient;
use str0m::{
    Event, Input, Rtc,
    change::SdpAnswer,
    media::{Direction, Mid, Simulcast, SimulcastLayer},
    net::Receive,
};
use tokio::net::UdpSocket;
use tokio::sync::{broadcast, mpsc};

#[derive(Debug, Clone)]
pub enum AgentEvent {
    MediaAdded,
    StateChanged(str0m::IceConnectionState),
    Connected,
}

pub struct AgentBuilder {
    api_base: Option<String>,
    room_id: Option<String>,
}

#[derive(Clone)]
pub struct Agent {
    events: broadcast::Sender<AgentEvent>,
}

impl Agent {
    pub async fn join(
        http_client: HttpClient,
        socket: UdpSocket,
        api_base: &str,
        room_id: &str,
    ) -> Result<Self> {
        let (event_tx, _event_rx) = broadcast::channel(128);
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

        let driver = AgentDriver {
            socket,
            event_tx: event_tx.clone(),
        };

        let driver_task = AssertUnwindSafe(driver.run()).catch_unwind();
        tokio::spawn(driver_task);

        let _ = HttpClient::new().delete(&location).send().await;

        Ok(Self {
            frame_tx,
            events: event_tx,
            video_mid,
        })
    }

    /// Subscribe to agent events
    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.events.subscribe()
    }

    /// Convenience: Send a raw H264 NALU.
    /// str0m handles RTP packetization/fragmentation automatically.
    pub async fn send_video(&self, nalu: impl Into<Bytes>) -> Result<()> {
        self.frame_tx
            .send(nalu.into())
            .await
            .map_err(|_| anyhow!("Agent closed"))
    }
}

struct AgentDriver {
    socket: UdpSocket,
    event_tx: broadcast::Sender<AgentEvent>,
}

impl AgentDriver {
    async fn run(mut self) {
        let mut buf = vec![0u8; 2000];
        let mut agent_rtc = rtc;

        loop {
            let now = std::time::Instant::now();
            let timeout = agent_rtc
                .poll_timeout()
                .map(|d| now + d)
                .unwrap_or(now + std::time::Duration::from_secs(1));

            tokio::select! {
                // Receive from SFU
                res = agent_socket.recv_from(&mut buf) => {
                    if let Ok((len, _)) = res {
                        let input = Input::Receive(
                            now,
                            Receive {
                                source: remote_addr,
                                destination: agent_socket.local_addr().unwrap(),
                                contents: (&buf[..len]).into(),
                            },
                        );
                        agent_rtc.handle_input(input).ok();
                    }
                }

                // Send Frame to SFU
                Some(frame) = frame_rx.recv() => {
                    if let Some(mid) = agent_video_mid {
                        if let Some(mut writer) = agent_rtc.writer(mid) {
                            writer.write(now, frame).ok();
                        }
                    }
                }

                // Handle str0m internal timers
                _ = tokio::time::sleep_until(timeout.into()) => {
                    agent_rtc.handle_input(Input::Timeout(now)).ok();
                }
            }

            // Drive Outgoing UDP
            while let Some(transmit) = agent_rtc.poll_transmit() {
                agent_socket
                    .send_to(&transmit.contents, transmit.destination)
                    .await
                    .ok();
            }

            // Handle str0m Events
            while let Some(event) = agent_rtc.poll_event() {
                match event {
                    Event::Connected => {
                        agent_events.send(AgentEvent::Connected).ok();
                    }
                    Event::IceConnectionStateChange(s) => {
                        agent_events.send(AgentEvent::StateChanged(s)).ok();
                    }
                    Event::MediaData(data) => {
                        agent_events.send(AgentEvent::VideoFrame(data.data)).ok();
                    }
                    _ => {}
                }
            }
        }

        // DELETE session on exit (Cleanup)
        #[allow(unreachable_code)]
        {
            let _ = HttpClient::new().delete(&location).send().await;
        }
    }
}
