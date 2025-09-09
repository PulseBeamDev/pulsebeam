use anyhow::{Context, Result, anyhow};
use reqwest::Client as HttpClient;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use str0m::{
    Candidate, IceCandidate, Rtc, RtcError,
    change::{SdpAnswer, SdpOffer},
    media::{Direction, MediaKind, Mid},
};
use tokio::sync::{mpsc, oneshot};

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
    cancel_tx: oneshot::Sender<()>,
    event_tx: mpsc::Sender<AgentEvent>,
    event_rx: mpsc::Receiver<AgentEvent>,
    rtp_tx: mpsc::Sender<(Mid, Vec<u8>)>,
    rtp_rx: mpsc::Receiver<(Mid, Vec<u8>)>,
    http_client: HttpClient,
}

impl Agent {
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
        let sdp_offer = SdpOffer::from_sdp_string(&offer.to_sdp_string())?;

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
        sdp.accept_answer(pending_offer, answer);

        // Start session event loop
        let session = Agent {
            rtc,
            endpoint,
            config,
            cancel_tx,
            event_tx,
            event_rx,
            rtp_tx,
            rtp_rx,
            http_client,
        };

        tokio::spawn({
            let mut session = session.clone();
            async move {
                session.run(cancel_rx).await.unwrap_or_else(|e| {
                    log::error!("Session error: {}", e);
                });
            }
        });

        Ok(session)
    }

    async fn run(&mut self, mut cancel_rx: oneshot::Receiver<()>) -> Result<()> {
        loop {
            tokio::select! {
                _ = &mut cancel_rx => {
                    self.disconnect().await?;
                    self.event_tx.send(AgentEvent::Disconnected).await?;
                    return Ok(());
                }
                event = self.rtc.poll_output() => {
                    match event {
                        Ok(str0m::Event::IceCandidate { candidate, .. }) => {
                            self.handle_ice_candidate(candidate).await?;
                        }
                        Ok(str0m::Event::MediaData { mid, data, .. }) => {
                            self.event_tx.send(AgentEvent::RtpReceived(mid, data)).await?;
                        }
                        Ok(str0m::Event::Connected) => {
                            self.event_tx.send(AgentEvent::Connected).await?;
                        }
                        Ok(_) => {}
                        Err(e) => return Err(anyhow!("RTC error: {}", e)),
                    }
                }
                rtp = self.rtp_rx.recv() => {
                    if let Some((mid, data)) = rtp {
                        self.rtc.write_packet(mid, data)?;
                    } else {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    async fn disconnect(&self) -> Result<()> {
        self.http_client
            .delete(&self.config.endpoint)
            .send()
            .await?;
        Ok(())
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
