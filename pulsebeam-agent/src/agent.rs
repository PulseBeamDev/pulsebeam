use anyhow::{Result, anyhow};
use bytes::Bytes;
use reqwest::Client as HttpClient;
use std::sync::Arc;
use str0m::{
    Event, Input, Rtc,
    media::{Direction, Mid},
    net::Receive,
};
use tokio::net::UdpSocket;
use tokio::sync::{broadcast, mpsc};

#[derive(Debug, Clone)]
pub enum AgentEvent {
    VideoFrame(Bytes),
    AudioFrame(Bytes),
    StateChanged(str0m::IceConnectionState),
    Connected,
}

#[derive(Clone)]
pub struct WebRtcAgent {
    frame_tx: mpsc::Sender<Bytes>,
    events: broadcast::Sender<AgentEvent>,
    pub video_mid: Option<Mid>,
}

impl WebRtcAgent {
    /// Mirrored from your JS `start()` logic.
    /// Performs HTTP POST, sets up transceivers, and spawns the worker.
    pub async fn join(
        api_base: &str,
        room_id: &str,
        publish: bool,
        subscribe: bool,
    ) -> Result<Self> {
        let http = HttpClient::new();
        let (event_tx, _event_rx) = broadcast::channel(128);
        let (frame_tx, mut frame_rx) = mpsc::channel::<Bytes>(100);

        // 1. Setup str0m (ice_lite should be true for client mode)
        let mut rtc = Rtc::builder().set_ice_lite(true).build();

        let mut video_mid = None;

        // 2. Configure Transceivers (Matching your JS Encodings)
        if publish {
            let mid = rtc.media().add_media(
                Direction::SendOnly,
                str0m::media::MediaKind::Video,
                "video".to_string(),
            );

            // Configure H264 codec
            if let Some(m) = rtc.media_mut(mid) {
                m.set_codec(str0m::media::CodecConfig::H264);

                // Add Simulcast Rids: q (1/4), h (1/2), f (1/1)
                m.set_rids(vec!["q".into(), "h".into(), "f".into()]);
            }

            video_mid = Some(mid);
        }

        if subscribe {
            rtc.media().add_media(
                Direction::RecvOnly,
                str0m::media::MediaKind::Video,
                "video-recv".to_string(),
            );
        }

        // 3. Handshake (POST Offer -> Get Answer)
        let offer = rtc.sdp_api().create_offer();
        let sdp_offer = offer.to_sdp_string();

        let res = http
            .post(format!("{}/api/v1/rooms/{}", api_base, room_id))
            .header("Content-Type", "application/sdp")
            .body(sdp_offer)
            .send()
            .await?;

        if !res.status().is_success() {
            return Err(anyhow!("SFU Join Failed: {}", res.status()));
        }

        let location = res
            .headers()
            .get("Location")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("No Location header for cleanup"))?;

        let sdp_answer = res.text().await?;

        // 4. Finalize WebRTC State
        rtc.sdp_api().accept_answer(sdp_answer)?;

        // 5. Initialize Networking (Compatible with Turmoil/Tokio)
        let socket = Arc::new(UdpSocket::bind("0.0.0.0:0").await?);
        let remote_addr = rtc
            .direct_api()
            .remote_addr()
            .ok_or_else(|| anyhow!("No ICE candidate"))?;

        // 6. Spawn the Background Agent Worker
        let agent_socket = socket.clone();
        let agent_events = event_tx.clone();
        let agent_video_mid = video_mid;

        tokio::spawn(async move {
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
        });

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
