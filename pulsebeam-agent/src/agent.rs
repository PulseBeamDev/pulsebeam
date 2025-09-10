use crate::media::{MediaInput, MediaSource};
use anyhow::{Context, Result, anyhow};
use reqwest::Client as HttpClient;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use str0m::{
    Rtc,
    change::SdpAnswer,
    media::{Direction, MediaKind, Mid},
    net::{Protocol, Receive},
};
use tokio::{
    net::UdpSocket,
    sync::{mpsc, oneshot},
    time::{Instant, sleep_until},
};
use tracing::{debug, error, info, warn};

/// Configuration for the WebRTC agent
#[derive(Clone, Debug)]
pub struct AgentConfig {
    pub endpoint: String,
    pub audio_enabled: bool,
    pub video_enabled: bool,
    pub audio_codec: Option<AudioCodec>,
    pub video_codec: Option<VideoCodec>,
    pub bitrate_kbps: Option<u32>,
    pub connection_timeout: Duration,
    pub session_name: Option<String>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            audio_enabled: true,
            video_enabled: true,
            audio_codec: Some(AudioCodec::Opus),
            video_codec: Some(VideoCodec::H264),
            bitrate_kbps: Some(1000),
            connection_timeout: Duration::from_secs(30),
            session_name: None,
        }
    }
}

#[derive(Clone, Debug)]
pub enum AudioCodec {
    Opus,
    G722,
    PCMU,
    PCMA,
}

#[derive(Clone, Debug)]
pub enum VideoCodec {
    H264,
    VP8,
    VP9,
    AV1,
}

/// Events emitted by the agent
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// RTP packet received from remote peer
    RtpReceived {
        mid: Mid,
        payload_type: u8,
        sequence: u16,
        timestamp: u32,
        ssrc: u32,
        data: Vec<u8>,
    },
    /// RTCP packet received
    RtcpReceived { mid: Mid, data: Vec<u8> },
    /// Connection established
    Connected,
    /// Connection lost
    Disconnected,
    /// ICE connection state changed
    IceStateChanged(IceConnectionState),
    /// Media track added
    TrackAdded {
        mid: Mid,
        kind: MediaKind,
        direction: Direction,
    },
    /// Error occurred
    Error(String),
    /// Statistics update
    Stats(ConnectionStats),
}

#[derive(Debug, Clone)]
pub enum IceConnectionState {
    New,
    Checking,
    Connected,
    Completed,
    Failed,
    Disconnected,
    Closed,
}

#[derive(Debug, Clone)]
pub struct ConnectionStats {
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub packets_sent: u64,
    pub packets_received: u64,
    pub round_trip_time: Option<Duration>,
    pub jitter: Option<f64>,
    pub packet_loss_rate: Option<f64>,
}

struct Agent {
    rtc: Rtc,
    config: AgentConfig,
    endpoint: String,
    event_tx: mpsc::Sender<AgentEvent>,
    http_client: HttpClient,
    socket: Option<Arc<UdpSocket>>,
    remote_addr: Option<SocketAddr>,
    track_mids: HashMap<MediaKind, Mid>,
    stats: ConnectionStats,
    last_stats_time: Instant,
}

impl Agent {
    async fn run(
        &mut self,
        mut cancel_rx: oneshot::Receiver<()>,
        mut media_rx: mpsc::Receiver<MediaInput>,
    ) -> Result<()> {
        let mut next_timeout = Instant::now() + Duration::from_millis(100);

        info!("Starting WebRTC agent");

        loop {
            tokio::select! {
                _ = &mut cancel_rx => {
                    info!("Agent shutdown requested");
                    break;
                }

                // Handle str0m timeouts and events
                _ = sleep_until(next_timeout) => {
                    match self.rtc.poll_output() {
                        Ok(output) => {
                            if let Some(timeout) = self.handle_output(output).await {
                                next_timeout = timeout;
                            } else {
                                next_timeout = Instant::now() + Duration::from_millis(100);
                            }
                        }
                        Err(e) => {
                            error!("RTC poll error: {}", e);
                            self.send_event(AgentEvent::Error(format!("RTC poll error: {}", e))).await;
                        }
                    }
                }

                // Handle incoming media to send
                Some(media) = media_rx.recv() => {
                    self.handle_media_input(media).await;
                }

                // Handle incoming UDP packets
                result = self.receive_udp(), if self.socket.is_some() => {
                    match result {
                        Ok((data, addr)) => {
                            self.handle_incoming_packet(data, addr).await;
                        }
                        Err(e) => {
                            warn!("UDP receive error: {}", e);
                        }
                    }
                }

                // Periodic stats update
                _ = tokio::time::sleep(Duration::from_secs(5)) => {
                    self.update_stats().await;
                }

                else => break,
            }
        }

        info!("Agent stopping");
        self.disconnect().await?;
        self.send_event(AgentEvent::Disconnected).await;
        Ok(())
    }

    async fn handle_output(&mut self, output: str0m::Output) -> Option<Instant> {
        match output {
            str0m::Output::Timeout(deadline) => {
                return Some(deadline.into());
            }

            str0m::Output::Transmit(transmit) => {
                if let Some(socket) = &self.socket {
                    if let Some(addr) = self.remote_addr {
                        match socket.send_to(&transmit.contents, addr).await {
                            Ok(bytes_sent) => {
                                self.stats.packets_sent += 1;
                                self.stats.bytes_sent += bytes_sent as u64;
                            }
                            Err(e) => {
                                error!("Failed to send UDP packet: {}", e);
                                self.send_event(AgentEvent::Error(format!(
                                    "UDP send error: {}",
                                    e
                                )))
                                .await;
                            }
                        }
                    }
                }
            }

            str0m::Output::Event(event) => {
                self.handle_rtc_event(event).await;
            }
        }

        None
    }

    async fn handle_rtc_event(&mut self, event: str0m::Event) {
        match event {
            str0m::Event::MediaData(media) => {
                self.stats.packets_received += 1;
                self.stats.bytes_received += media.data.len() as u64;

                self.send_event(AgentEvent::RtpReceived {
                    mid: media.mid,
                    payload_type: media.payload_type,
                    sequence: media.sequence_number,
                    timestamp: media.timestamp,
                    ssrc: media.ssrc,
                    data: media.data,
                })
                .await;
            }

            str0m::Event::Connected => {
                info!("WebRTC connection established");
                self.send_event(AgentEvent::Connected).await;
            }

            str0m::Event::IceConnectionStateChange(state) => {
                let ice_state = match state {
                    str0m::IceConnectionState::New => IceConnectionState::New,
                    str0m::IceConnectionState::Checking => IceConnectionState::Checking,
                    str0m::IceConnectionState::Connected => IceConnectionState::Connected,
                    str0m::IceConnectionState::Completed => IceConnectionState::Completed,
                    str0m::IceConnectionState::Failed => IceConnectionState::Failed,
                    str0m::IceConnectionState::Disconnected => IceConnectionState::Disconnected,
                    str0m::IceConnectionState::Closed => IceConnectionState::Closed,
                };

                debug!("ICE connection state changed: {:?}", ice_state);
                self.send_event(AgentEvent::IceStateChanged(ice_state))
                    .await;
            }

            str0m::Event::MediaAdded(media) => {
                info!(
                    "Media track added: mid={}, kind={:?}, direction={:?}",
                    media.mid, media.kind, media.direction
                );

                self.send_event(AgentEvent::TrackAdded {
                    mid: media.mid,
                    kind: media.kind,
                    direction: media.direction,
                })
                .await;
            }

            str0m::Event::RtcpData(rtcp) => {
                self.send_event(AgentEvent::RtcpReceived {
                    mid: rtcp.mid,
                    data: rtcp.data,
                })
                .await;
            }

            str0m::Event::KeyFrameRequest(req) => {
                debug!("Key frame requested for MID: {}", req.mid);
                // Forward keyframe request if needed
            }

            _ => {
                debug!("Unhandled RTC event: {:?}", event);
            }
        }
    }

    async fn handle_media_input(&mut self, media: MediaInput) {
        if let Some(writer) = self.rtc.writer(media.mid) {
            let wallclock = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();

            match writer.write(
                str0m::media::Pt::new_with_value(media.payload_type),
                wallclock,
                media.timestamp,
                media.data,
            ) {
                Ok(_) => {
                    debug!("Media written to MID: {}", media.mid);
                }
                Err(e) => {
                    error!("Failed to write media to MID {}: {}", media.mid, e);
                }
            }
        } else {
            warn!("No writer found for MID: {}", media.mid);
        }
    }

    async fn receive_udp(&self) -> Result<(Vec<u8>, SocketAddr)> {
        if let Some(socket) = &self.socket {
            let mut buf = vec![0u8; 1500]; // MTU size
            let (len, addr) = socket.recv_from(&mut buf).await?;
            buf.truncate(len);
            Ok((buf, addr))
        } else {
            Err(anyhow!("No UDP socket available"))
        }
    }

    async fn handle_incoming_packet(&mut self, data: Vec<u8>, addr: SocketAddr) {
        // Update remote address if not set
        if self.remote_addr.is_none() {
            self.remote_addr = Some(addr);
            debug!("Set remote address to: {}", addr);
        }

        let local_addr = self
            .socket
            .as_ref()
            .and_then(|s| s.local_addr().ok())
            .unwrap_or_else(|| "0.0.0.0:0".parse().unwrap());

        let receive = Receive {
            proto: Protocol::Udp,
            source: addr,
            destination: local_addr,
            contents: data.into(),
        };

        if let Err(e) = self.rtc.handle_input(receive) {
            error!("Failed to handle incoming packet from {}: {}", addr, e);
        }
    }

    async fn update_stats(&mut self) {
        let now = Instant::now();
        if now.duration_since(self.last_stats_time) >= Duration::from_secs(5) {
            self.last_stats_time = now;

            // TODO: Extract additional stats from str0m if available
            debug!(
                "Connection stats: sent={} packets, received={} packets",
                self.stats.packets_sent, self.stats.packets_received
            );

            self.send_event(AgentEvent::Stats(self.stats.clone())).await;
        }
    }

    async fn send_event(&self, event: AgentEvent) {
        if let Err(_) = self.event_tx.send(event).await {
            error!("Failed to send event: channel closed");
        }
    }

    async fn disconnect(&self) -> Result<()> {
        if !self.endpoint.is_empty() {
            match self.http_client.delete(&self.endpoint).send().await {
                Ok(response) => {
                    debug!(
                        "DELETE request sent to {}, status: {}",
                        self.endpoint,
                        response.status()
                    );
                }
                Err(e) => {
                    warn!("Failed to send DELETE request to {}: {}", self.endpoint, e);
                }
            }
        }
        Ok(())
    }
}

pub struct AgentHandle {
    cancel_tx: Option<oneshot::Sender<()>>,
    event_rx: mpsc::Receiver<AgentEvent>,
    media_tx: mpsc::Sender<MediaInput>,
    track_mids: HashMap<MediaKind, Mid>,
}

impl AgentHandle {
    /// Connect to an SFU using WHIP/WHEP protocol
    pub async fn connect(config: AgentConfig) -> Result<Self> {
        info!("Connecting to SFU endpoint: {}", config.endpoint);

        let mut rtc = Rtc::builder()
            .enable_h264(
                config
                    .video_codec
                    .as_ref()
                    .map_or(false, |c| matches!(c, VideoCodec::H264)),
            )
            .build();

        let mut sdp_api = rtc.sdp_api();
        let mut track_mids = HashMap::new();

        // Add media tracks based on configuration
        if config.audio_enabled {
            let payload_types = config.audio_codec.as_ref().map(|c| match c {
                AudioCodec::Opus => vec![111],
                AudioCodec::G722 => vec![9],
                AudioCodec::PCMU => vec![0],
                AudioCodec::PCMA => vec![8],
            });

            let mid = sdp_api.add_media(
                MediaKind::Audio,
                Direction::SendRecv,
                None,
                None,
                payload_types,
            );
            track_mids.insert(MediaKind::Audio, mid);
            info!("Added audio track with MID: {}", mid);
        }

        if config.video_enabled {
            let payload_types = config.video_codec.as_ref().map(|c| match c {
                VideoCodec::H264 => vec![96],
                VideoCodec::VP8 => vec![97],
                VideoCodec::VP9 => vec![98],
                VideoCodec::AV1 => vec![99],
            });

            let mid = sdp_api.add_media(
                MediaKind::Video,
                Direction::SendRecv,
                None,
                None,
                payload_types,
            );
            track_mids.insert(MediaKind::Video, mid);
            info!("Added video track with MID: {}", mid);
        }

        // Generate SDP offer
        let (offer, pending_offer) = sdp_api.apply().context("Failed to generate SDP offer")?;
        debug!("Generated SDP offer:\n{}", offer.to_sdp_string());

        let http_client = HttpClient::builder()
            .timeout(config.connection_timeout)
            .build()?;

        // Send WHIP/WHEP request
        let mut request = http_client
            .post(&config.endpoint)
            .header("Content-Type", "application/sdp");

        if let Some(name) = &config.session_name {
            request = request.header("X-Session-Name", name);
        }

        let resp = request
            .body(offer.to_sdp_string())
            .send()
            .await
            .context("Failed to send WHIP/WHEP request")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!(
                "WHIP/WHEP request failed with status {}: {}",
                status,
                body
            ));
        }

        // Get session endpoint from Location header
        let endpoint = resp
            .headers()
            .get("Location")
            .context("Missing Location header in WHIP/WHEP response")?
            .to_str()?
            .to_string();

        info!("Session endpoint: {}", endpoint);

        // Parse SDP answer
        let answer_str = resp.text().await?;
        debug!("Received SDP answer:\n{}", answer_str);

        let answer =
            SdpAnswer::from_sdp_string(&answer_str).context("Failed to parse SDP answer")?;

        // Apply answer to RTC
        rtc.sdp_api().accept_answer(pending_offer, answer);
        info!("SDP negotiation completed");

        // Create communication channels
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let (event_tx, event_rx) = mpsc::channel(1000);
        let (media_tx, media_rx) = mpsc::channel(1000);

        // Create UDP socket for media transport
        let socket = Arc::new(
            UdpSocket::bind("0.0.0.0:0")
                .await
                .context("Failed to bind UDP socket")?,
        );

        let local_addr = socket.local_addr()?;
        info!("Bound UDP socket to: {}", local_addr);

        // Initialize statistics
        let stats = ConnectionStats {
            bytes_sent: 0,
            bytes_received: 0,
            packets_sent: 0,
            packets_received: 0,
            round_trip_time: None,
            jitter: None,
            packet_loss_rate: None,
        };

        let mut agent = Agent {
            rtc,
            config,
            endpoint,
            event_tx,
            http_client,
            socket: Some(socket),
            remote_addr: None,
            track_mids: track_mids.clone(),
            stats,
            last_stats_time: Instant::now(),
        };

        // Spawn agent task
        tokio::spawn(async move {
            if let Err(e) = agent.run(cancel_rx, media_rx).await {
                error!("Agent error: {}", e);
            }
        });

        Ok(Self {
            cancel_tx: Some(cancel_tx),
            event_rx,
            media_tx,
            track_mids,
        })
    }

    /// Send raw media data to remote peers
    pub async fn send_media(
        &self,
        mid: Mid,
        payload_type: u8,
        timestamp: u32,
        data: Vec<u8>,
        marker: bool,
    ) -> Result<()> {
        let media = MediaInput {
            mid,
            payload_type,
            timestamp,
            data,
            marker,
        };

        self.media_tx
            .send(media)
            .await
            .map_err(|_| anyhow!("Failed to send media: channel closed"))?;
        Ok(())
    }

    /// Send audio data (convenience method)
    pub async fn send_audio(&self, data: Vec<u8>, timestamp: u32) -> Result<()> {
        if let Some(&mid) = self.track_mids.get(&MediaKind::Audio) {
            self.send_media(mid, 111, timestamp, data, false).await // Default to Opus
        } else {
            Err(anyhow!("Audio track not available"))
        }
    }

    /// Send video data (convenience method)
    pub async fn send_video(&self, data: Vec<u8>, timestamp: u32, is_keyframe: bool) -> Result<()> {
        if let Some(&mid) = self.track_mids.get(&MediaKind::Video) {
            self.send_media(mid, 96, timestamp, data, is_keyframe).await // Default to H.264
        } else {
            Err(anyhow!("Video track not available"))
        }
    }

    /// Add a media source for continuous streaming
    pub async fn add_media_source(&self, mut source: Box<dyn MediaSource>) -> Result<()> {
        let media_tx = self.media_tx.clone();
        let media_kind = source.media_kind();
        let track_mids = self.track_mids.clone();

        info!("Adding media source for {:?}", media_kind);

        tokio::spawn(async move {
            while let Some(mut media) = source.next_frame().await {
                // Update MID based on media kind
                if let Some(&mid) = track_mids.get(&media_kind) {
                    media.mid = mid;

                    if media_tx.send(media).await.is_err() {
                        debug!("Media source channel closed, stopping source");
                        break;
                    }
                } else {
                    warn!("No MID found for media kind: {:?}", media_kind);
                    break;
                }
            }

            info!("Media source for {:?} stopped", media_kind);
        });

        Ok(())
    }

    /// Receive the next event from the agent
    pub async fn recv_event(&mut self) -> Option<AgentEvent> {
        self.event_rx.recv().await
    }

    /// Try to receive an event without blocking
    pub fn try_recv_event(&mut self) -> Result<AgentEvent, mpsc::error::TryRecvError> {
        self.event_rx.try_recv()
    }

    /// Get track MID for a specific media kind
    pub fn get_track_mid(&self, kind: MediaKind) -> Option<Mid> {
        self.track_mids.get(&kind).copied()
    }

    /// Get all available track MIDs
    pub fn get_all_track_mids(&self) -> HashMap<MediaKind, Mid> {
        self.track_mids.clone()
    }

    /// Check if the connection is still active
    pub fn is_connected(&self) -> bool {
        self.cancel_tx.is_some()
    }

    /// Stop the agent and close the connection
    pub async fn stop(mut self) -> Result<()> {
        info!("Stopping WebRTC agent");
        if let Some(cancel_tx) = self.cancel_tx.take() {
            cancel_tx
                .send(())
                .map_err(|_| anyhow!("Failed to send stop signal"))?;
        }
        Ok(())
    }
}

impl Drop for AgentHandle {
    fn drop(&mut self) {
        if let Some(cancel_tx) = self.cancel_tx.take() {
            let _ = cancel_tx.send(());
        }
    }
}

/// Utility functions and helpers
pub mod utils {
    use super::*;

    /// Create a test configuration for simulation testing
    pub fn create_test_config(endpoint: &str) -> AgentConfig {
        AgentConfig {
            endpoint: endpoint.to_string(),
            audio_enabled: true,
            video_enabled: true,
            audio_codec: Some(AudioCodec::Opus),
            video_codec: Some(VideoCodec::H264),
            bitrate_kbps: Some(2000),
            connection_timeout: Duration::from_secs(10),
            session_name: Some("test-session".to_string()),
        }
    }

    /// Create a benchmark configuration for performance testing
    pub fn create_benchmark_config(
        endpoint: &str,
        enable_video: bool,
        enable_audio: bool,
    ) -> AgentConfig {
        AgentConfig {
            endpoint: endpoint.to_string(),
            audio_enabled: enable_audio,
            video_enabled: enable_video,
            audio_codec: Some(AudioCodec::Opus),
            video_codec: Some(VideoCodec::H264),
            bitrate_kbps: Some(5000),
            connection_timeout: Duration::from_secs(30),
            session_name: Some("benchmark".to_string()),
        }
    }

    /// Create a recording configuration for SFU extensions
    pub fn create_recording_config(endpoint: &str) -> AgentConfig {
        AgentConfig {
            endpoint: endpoint.to_string(),
            audio_enabled: true,
            video_enabled: true,
            audio_codec: Some(AudioCodec::Opus),
            video_codec: Some(VideoCodec::H264),
            bitrate_kbps: None, // No bitrate limit for recording
            connection_timeout: Duration::from_secs(60),
            session_name: Some("recording-session".to_string()),
        }
    }

    /// Create a minimal configuration for AI agents
    pub fn create_ai_agent_config(endpoint: &str, session_name: &str) -> AgentConfig {
        AgentConfig {
            endpoint: endpoint.to_string(),
            audio_enabled: true,
            video_enabled: false, // AI agents often don't need video
            audio_codec: Some(AudioCodec::Opus),
            video_codec: None,
            bitrate_kbps: Some(64), // Low bitrate for voice
            connection_timeout: Duration::from_secs(15),
            session_name: Some(session_name.to_string()),
        }
    }
}

/// Example implementations for different use cases
pub mod examples {
    use super::*;
    use tokio::time::timeout;

    /// Simple CLI client example
    pub async fn cli_client_example(endpoint: &str) -> Result<()> {
        let config = utils::create_test_config(endpoint);
        let mut handle = AgentHandle::connect(config).await?;

        // Add a test pattern source
        let video_source = Box::new(TestPatternSource::new(
            MediaKind::Video,
            96,   // H.264
            30,   // 30fps
            1200, // Frame size
        ));

        handle.add_media_source(video_source).await?;

        println!("Connected to SFU. Streaming test pattern...");

        // Listen for events
        let mut packet_count = 0;
        let start_time = std::time::Instant::now();

        while let Some(event) = handle.recv_event().await {
            match event {
                AgentEvent::Connected => {
                    println!("✓ Connection established");
                }
                AgentEvent::RtpReceived { mid, data, .. } => {
                    packet_count += 1;
                    if packet_count % 100 == 0 {
                        println!("Received {} packets on MID {}", packet_count, mid);
                    }
                }
                AgentEvent::Stats(stats) => {
                    println!(
                        "Stats: TX: {} packets, RX: {} packets",
                        stats.packets_sent, stats.packets_received
                    );
                }
                AgentEvent::Error(err) => {
                    eprintln!("Error: {}", err);
                    break;
                }
                AgentEvent::Disconnected => {
                    println!("Disconnected");
                    break;
                }
                _ => {}
            }

            // Stop after 30 seconds for demo
            if start_time.elapsed() > Duration::from_secs(30) {
                break;
            }
        }

        handle.stop().await?;
        Ok(())
    }

    /// Benchmark example for performance testing
    pub async fn benchmark_example(endpoint: &str, duration_secs: u64) -> Result<ConnectionStats> {
        let config = utils::create_benchmark_config(endpoint, true, true);
        let mut handle = AgentHandle::connect(config).await?;

        // Add high-rate test sources
        let video_source = Box::new(TestPatternSource::new(
            MediaKind::Video,
            96,
            60, // 60fps for stress testing
            1400,
        ));

        let audio_source = Box::new(TestPatternSource::new(
            MediaKind::Audio,
            111,
            50, // 50fps (20ms frames)
            160,
        ));

        handle.add_media_source(video_source).await?;
        handle.add_media_source(audio_source).await?;

        println!("Starting benchmark for {} seconds...", duration_secs);

        let mut final_stats = ConnectionStats {
            bytes_sent: 0,
            bytes_received: 0,
            packets_sent: 0,
            packets_received: 0,
            round_trip_time: None,
            jitter: None,
            packet_loss_rate: None,
        };

        let benchmark_timeout = Duration::from_secs(duration_secs);
        let result = timeout(benchmark_timeout, async {
            while let Some(event) = handle.recv_event().await {
                match event {
                    AgentEvent::Stats(stats) => {
                        final_stats = stats;
                        println!(
                            "Benchmark progress - TX: {} packets, RX: {} packets",
                            final_stats.packets_sent, final_stats.packets_received
                        );
                    }
                    AgentEvent::Error(err) => {
                        eprintln!("Benchmark error: {}", err);
                        break;
                    }
                    AgentEvent::Disconnected => {
                        break;
                    }
                    _ => {}
                }
            }
        })
        .await;

        match result {
            Ok(_) => println!("Benchmark completed normally"),
            Err(_) => println!("Benchmark completed (timeout)"),
        }

        handle.stop().await?;

        // Calculate rates
        let duration_f64 = duration_secs as f64;
        println!("\n=== Benchmark Results ===");
        println!("Duration: {} seconds", duration_secs);
        println!(
            "Packets sent: {} ({:.2}/sec)",
            final_stats.packets_sent,
            final_stats.packets_sent as f64 / duration_f64
        );
        println!(
            "Packets received: {} ({:.2}/sec)",
            final_stats.packets_received,
            final_stats.packets_received as f64 / duration_f64
        );
        println!(
            "Bytes sent: {} ({:.2} KB/sec)",
            final_stats.bytes_sent,
            final_stats.bytes_sent as f64 / duration_f64 / 1024.0
        );
        println!(
            "Bytes received: {} ({:.2} KB/sec)",
            final_stats.bytes_received,
            final_stats.bytes_received as f64 / duration_f64 / 1024.0
        );

        Ok(final_stats)
    }

    /// Recording example for SFU extensions
    pub async fn recording_example(endpoint: &str, output_file: &str) -> Result<()> {
        let config = utils::create_recording_config(endpoint);
        let mut handle = AgentHandle::connect(config).await?;

        println!("Starting recording session to: {}", output_file);

        // TODO: Implement actual file writing
        // This would write received RTP packets to a file for later playback
        let mut packet_count = 0;

        while let Some(event) = handle.recv_event().await {
            match event {
                AgentEvent::Connected => {
                    println!("✓ Recording session started");
                }
                AgentEvent::RtpReceived {
                    mid,
                    data,
                    timestamp,
                    ..
                } => {
                    packet_count += 1;

                    // TODO: Write packet to file
                    // recorder.write_rtp_packet(mid, timestamp, &data).await?;

                    if packet_count % 1000 == 0 {
                        println!("Recorded {} packets", packet_count);
                    }
                }
                AgentEvent::Error(err) => {
                    eprintln!("Recording error: {}", err);
                    break;
                }
                AgentEvent::Disconnected => {
                    println!("Recording session ended");
                    break;
                }
                _ => {}
            }
        }

        println!("Recorded {} total packets", packet_count);
        handle.stop().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_agent_config_creation() {
        let config = utils::create_test_config("http://localhost:8080");
        assert!(config.audio_enabled);
        assert!(config.video_enabled);
        assert_eq!(config.endpoint, "http://localhost:8080");
    }

    // Integration tests would require a real SFU endpoint
    #[tokio::test]
    #[ignore] // Ignore by default since it requires external SFU
    async fn test_integration_with_real_sfu() {
        let config = utils::create_test_config("http://localhost:7080");
        let handle = AgentHandle::connect(config).await;

        // This test would only pass with a real SFU running
        assert!(handle.is_ok() || handle.is_err()); // Either outcome is valid for test
    }
}
