use anyhow::{Context, Result, anyhow};
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use str0m::{
    Rtc,
    change::SdpAnswer,
    media::{Direction, MediaKind, Mid},
};
use tokio::{
    sync::{mpsc, oneshot},
    time::Instant,
};

#[derive(Clone)]
pub struct AgentConfig {
    pub endpoint: String,
    pub audio_enabled: bool,
    pub video_enabled: bool,
}

#[derive(Debug)]
pub enum AgentEvent {
    RtpReceived(Mid, Vec<u8>),
    Connected,
    Disconnected,
}

#[derive(Serialize, Deserialize)]
struct WhipResponse {
    sdp: String,
    #[serde(rename = "location")]
    _location: String,
}

pub struct Agent {
    rtc: Rtc,
    config: AgentConfig,
    endpoint: String,
    event_tx: mpsc::Sender<AgentEvent>,
    rtp_rx: mpsc::Receiver<(Mid, Vec<u8>)>,
    http_client: HttpClient,
}

impl Agent {
    async fn run(&mut self, mut cancel_rx: oneshot::Receiver<()>) -> Result<()> {
        loop {
            tokio::select! {
                _ = &mut cancel_rx => {
                    break;
                }
                Ok(output) = self.rtc.poll_output() => {
                    self.handle_output(output).await;
                }
                Some(rtp) = self.rtp_rx.recv() => {
                    self.handle_rtp_rx(rtp).await;
                }
                else => break,
            }
        }

        self.disconnect().await?;
        self.event_tx.send(AgentEvent::Disconnected).await?;
        Ok(())
    }

    async fn handle_output(&mut self, output: str0m::Output) -> Option<Instant> {
        match output {
            str0m::Output::Timeout(deadline) => return Ok(Some(deadline.into())),
            str0m::Output::Transmit(data) => {}
            str0m::Output::Event(event) => match event {
                str0m::Event::MediaData(media) => {
                    self.event_tx
                        .send(AgentEvent::RtpReceived(media.mid, media.data))
                        .await?;
                }
                str0m::Event::Connected => {
                    self.event_tx.send(AgentEvent::Connected).await?;
                }
                _ => {}
            },
        }

        None
    }

    async fn handle_rtp_rx(&mut self, rtp: (Mid, Vec<u8>)) {
        if let Some(writer) = self.rtc.writer(rtp.0) {
            writer.write(pt, wallclock, rtp_time, data)
        }
    }

    async fn disconnect(&self) -> Result<()> {
        self.http_client
            .delete(&self.config.endpoint)
            .send()
            .await?;
        Ok(())
    }
}

pub struct AgentHandle {
    cancel_tx: oneshot::Sender<()>,
    event_rx: mpsc::Receiver<AgentEvent>,
    rtp_tx: mpsc::Sender<(Mid, Vec<u8>)>,
}

impl AgentHandle {
    pub async fn connect(config: AgentConfig) -> Result<Self> {
        let mut rtc = Rtc::builder().enable_h264(true).build();
        let mut sdp = rtc.sdp_api();

        // Configure media tracks
        let mut mids = HashMap::new();
        if config.audio_enabled {
            let mid = sdp.add_media(MediaKind::Audio, Direction::SendOnly, None, None, None);
            mids.insert(MediaKind::Audio, mid);
        }
        if config.video_enabled {
            let mid = sdp.add_media(MediaKind::Video, Direction::SendOnly, None, None, None);
            mids.insert(MediaKind::Video, mid);
        }

        let (cancel_tx, cancel_rx) = oneshot::channel();
        let (event_tx, event_rx) = mpsc::channel(100);
        let (rtp_tx, rtp_rx) = mpsc::channel(100);
        let http_client = HttpClient::new();

        // Perform WHIP/WHEP handshake
        let (offer, pending_offer) = sdp.apply().context("expect an offer")?;

        let resp = http_client
            .post(&config.endpoint)
            .header("Content-Type", "application/sdp")
            .body(offer.to_sdp_string())
            .send()
            .await?;
        // TODO: resolve URL
        let endpoint = resp
            .headers()
            .get("Location")
            .context("expected the Location header to be set to the session endpoint")?
            .to_str()?
            .to_string();

        let answer_str = resp.text().await?;
        let answer = SdpAnswer::from_sdp_string(&answer_str)?;
        let sdp = rtc.sdp_api();
        sdp.accept_answer(pending_offer, answer);

        // Start session event loop
        let mut agent = Agent {
            rtc,
            endpoint,
            config,
            event_tx,
            rtp_rx,
            http_client,
        };

        let agent_handle = Self {
            cancel_tx,
            event_rx,
            rtp_tx,
        };

        tokio::spawn(async move {
            agent.run(cancel_rx).await.unwrap_or_else(|e| {
                tracing::error!("Session error: {}", e);
            });
        });

        Ok(agent_handle)
    }

    pub async fn send_rtp(&self, mid: Mid, rtp_packet: Vec<u8>) -> Result<()> {
        self.rtp_tx.send((mid, rtp_packet)).await?;
        Ok(())
    }

    pub async fn recv_event(&mut self) -> Option<AgentEvent> {
        self.event_rx.recv().await
    }

    pub async fn stop(self) -> Result<()> {
        self.cancel_tx
            .send(())
            .map_err(|_| anyhow!("Failed to cancel session"))?;
        Ok(())
    }
}
