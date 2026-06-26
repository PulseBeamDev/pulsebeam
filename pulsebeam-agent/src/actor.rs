use crate::api::{ApiError, CreateParticipantRequest, HttpApiClient};
use crate::manager::{Subscription, SubscriptionManager};
use crate::media::{KeyframeNotifier, KeyframeReceiver};
use crate::tcp::TcpSession;
use crate::{MediaFrame, TransceiverDirection};
use futures_lite::StreamExt;
use http::Uri;
use pulsebeam_core::net::UdpSocket;
use pulsebeam_proto::prelude::*;
use pulsebeam_proto::rtp_extensions;
use pulsebeam_proto::signaling::Track;
use pulsebeam_proto::{namespace, signaling};
use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use str0m::IceConnectionState;
use str0m::bwe::{Bitrate, BweKind};
use str0m::channel::{ChannelConfig, ChannelData, ChannelId, Reliability};
use str0m::media::{KeyframeRequestKind, Rid, Simulcast, SimulcastLayer};
use str0m::rtp::{AbsCaptureTime, Extension};
use str0m::{
    Candidate, Event, Input, Output, Rtc,
    media::{Direction, MediaAdded, MediaKind, Mid},
    net::{Protocol, Receive, TcpType},
};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_stream::StreamMap;
use tokio_stream::wrappers::ReceiverStream;

const MIN_QUANTA: Duration = Duration::from_millis(1);
const STATE_DEBOUNCE: Duration = Duration::from_millis(300);
const BWE_SLOW_INTERVAL: Duration = Duration::from_millis(200);
const BWE_DEFAULT: Bitrate = Bitrate::kbps(500);

// Debt thresholds (measured in ticks; one tick = BWE_SLOW_INTERVAL = 200 ms).
// Debt = allocated_bps − available_bps.
//
// • DEBT_LINGER_THRESHOLD  – fraction of available bandwidth above which debt is
//   non-trivial and starts counting toward DEBT_LINGER_MAX_TICKS.
// • DEBT_LINGER_MAX_TICKS  – consecutive ticks of sustained linger-level debt that
//   trigger a layer shed even when debt never hit the immediate threshold.
//   5 × 200 ms = 1 s of lingering excess triggers a drop.
// • DEBT_IMMEDIATE_THRESHOLD – fraction of available bandwidth at which debt is
//   large enough to shed a layer right away without waiting.
const DEBT_LINGER_THRESHOLD: f64 = 0.10;
const DEBT_LINGER_MAX_TICKS: u32 = 5;
const DEBT_IMMEDIATE_THRESHOLD: f64 = 0.25;
// Used as last_active_bps for paused layers with no prior measurement so the
// upgrade gate sees a realistic cost from the very first tick.
fn layer_seed_bps(rid: Option<Rid>) -> f64 {
    match rid_quality_rank(rid) {
        0 => 30_000.0,
        1 => 900_000.0,
        2 => 1_400_000.0,
        _ => 30_000.0,
    }
}

fn rid_quality_rank(rid: Option<Rid>) -> u8 {
    match rid.as_ref().map(|r| r.as_ref()) {
        Some("q") => 0,
        Some("h") => 1,
        Some("f") => 2,
        _ => 255,
    }
}

// Manages which simulcast send-layers are active based on TWCC bandwidth estimates.
// Layers are ordered by priority: q (highest) → h → f (lowest).
// The only control concept is *debt*: debt = allocated_bps − available_bps.
// Heavy or lingering debt causes the lowest-priority active layer to be shed.
// Once debt is gone, the next highest-priority paused layer is resumed if it fits.
const KEYFRAME_REQUEST_THROTTLE: Duration = Duration::from_secs(1);

struct BitrateEstimate {
    tick_start: Option<Instant>,
    accumulated_bytes: usize,
    raw_ticks: VecDeque<f64>,
    sma1_ticks: VecDeque<f64>,
    max_window_ticks: usize,
    baseline_bps: f64,
    fast_trend_bps: f64,
}

impl BitrateEstimate {
    const HEADROOM: f64 = 1.05;
    const TICK_MS: f64 = 500.0;

    pub fn new_with_seed(seed_bps: f64) -> Self {
        let mut raw_ticks = VecDeque::with_capacity(6);
        let mut sma1_ticks = VecDeque::with_capacity(6);
        for _ in 0..6 {
            raw_ticks.push_back(seed_bps);
            sma1_ticks.push_back(seed_bps);
        }
        Self {
            tick_start: None,
            accumulated_bytes: 0,
            raw_ticks,
            sma1_ticks,
            max_window_ticks: 6,
            baseline_bps: seed_bps,
            fast_trend_bps: seed_bps,
        }
    }

    pub fn record_bytes(&mut self, bytes: usize, now: Instant) {
        self.advance_time(now);
        self.accumulated_bytes += bytes;
    }

    pub fn poll(&mut self, current_time: Instant) {
        self.advance_time(current_time);
    }

    fn advance_time(&mut self, time: Instant) {
        let current_tick = *self.tick_start.get_or_insert(time);

        if time < current_tick + Duration::from_millis(Self::TICK_MS as u64) {
            return;
        }

        let elapsed = time.saturating_duration_since(current_tick);
        let ticks_passed = (elapsed.as_millis() / Self::TICK_MS as u128) as usize;

        let instant_bps = (self.accumulated_bytes as f64 * 8.0 * 1000.0) / Self::TICK_MS;
        self.push_tick(instant_bps);

        let empty_ticks = ticks_passed.saturating_sub(1).min(1000);
        for _ in 0..empty_ticks {
            self.push_tick(0.0);
        }

        self.accumulated_bytes = 0;
        self.tick_start = Some(
            current_tick + Duration::from_millis((ticks_passed as u64) * Self::TICK_MS as u64),
        );
    }

    fn push_tick(&mut self, bps: f64) {
        if self.raw_ticks.len() == self.max_window_ticks {
            self.raw_ticks.pop_front();
        }
        self.raw_ticks.push_back(bps);

        let sma1 = self.raw_ticks.iter().sum::<f64>() / self.raw_ticks.len() as f64;

        if self.sma1_ticks.len() == self.max_window_ticks {
            self.sma1_ticks.pop_front();
        }
        self.sma1_ticks.push_back(sma1);

        self.baseline_bps = self.sma1_ticks.iter().sum::<f64>() / self.sma1_ticks.len() as f64;

        let recent_len = self.raw_ticks.len();
        if recent_len >= 3 {
            let a = self.raw_ticks[recent_len - 1];
            let b = self.raw_ticks[recent_len - 2];
            let c = self.raw_ticks[recent_len - 3];
            self.fast_trend_bps = a.max(b.min(c)).min(b.max(c));
        } else {
            self.fast_trend_bps = 0.0;
        }
    }

    pub fn estimate_bps(&self) -> f64 {
        self.baseline_bps.max(self.fast_trend_bps) * Self::HEADROOM
    }
}

struct BitrateControllerConfig {
    pub min_bitrate: Bitrate,
    pub max_bitrate: Bitrate,
    pub default_bitrate: Bitrate,
    pub headroom_factor: f64,
    pub down_smoothing: f64,
    pub quantization_step: Bitrate,
    pub hysteresis: Bitrate,
}

impl Default for BitrateControllerConfig {
    fn default() -> Self {
        Self {
            min_bitrate: BWE_DEFAULT,
            max_bitrate: Bitrate::mbps(10),
            default_bitrate: BWE_DEFAULT,
            headroom_factor: 1.10,
            down_smoothing: 0.99,
            quantization_step: Bitrate::kbps(200),
            hysteresis: Bitrate::kbps(250),
        }
    }
}

impl BitrateControllerConfig {
    pub fn build(self) -> BitrateController {
        BitrateController::new(self)
    }
}

struct BitrateController {
    config: BitrateControllerConfig,
    current_bitrate: f64,
    down_estimate: f64,
}

impl BitrateController {
    pub fn new(config: BitrateControllerConfig) -> Self {
        let initial_bitrate = config.default_bitrate.as_f64();
        Self {
            config,
            current_bitrate: initial_bitrate,
            down_estimate: initial_bitrate,
        }
    }

    pub fn update(&mut self, desired_bitrate: Bitrate) -> Bitrate {
        let raw = desired_bitrate.as_f64() * self.config.headroom_factor;

        if raw > self.down_estimate {
            self.down_estimate = raw;
        } else {
            self.down_estimate = self.down_estimate * self.config.down_smoothing
                + raw * (1.0 - self.config.down_smoothing);
            if self.down_estimate - raw < 1.0 {
                self.down_estimate = raw;
            }
        }

        let deadband = self.config.hysteresis.as_f64();
        let step = self.config.quantization_step.as_f64();
        let target = ((self.down_estimate / step) - 1e-9).max(0.0).ceil() * step;

        if target > self.current_bitrate || self.current_bitrate - target >= deadband {
            self.current_bitrate = target;
        }

        self.current()
    }

    pub fn current(&self) -> Bitrate {
        let current_bitrate = self.current_bitrate.clamp(
            self.config.min_bitrate.as_f64(),
            self.config.max_bitrate.as_f64(),
        );
        Bitrate::from(current_bitrate)
    }
}

struct LayerController {
    available_bps: f64,
    last_keyframe_request: HashMap<(Mid, Option<Rid>), Instant>,
    // Sorted by priority: index 0 = q (highest), last = f (lowest).
    order: Vec<(Mid, Option<Rid>)>,
    states: HashMap<(Mid, Option<Rid>), LayerState>,
    notifiers: HashMap<(Mid, Option<Rid>), KeyframeNotifier>,
    /// Consecutive ticks where debt exceeded DEBT_LINGER_THRESHOLD.
    /// Resets to 0 whenever debt falls below the threshold or a layer is shed.
    debt_ticks: u32,
}

struct LayerState {
    /// Bitrate estimate derived from the server-side `BitrateEstimate`.
    /// Uses 500 ms tick windows with a triangular SMA and fast trend detector.
    bps: f64,
    paused: bool,
    estimate: BitrateEstimate,
}

impl LayerController {
    fn new() -> Self {
        Self {
            available_bps: f64::MAX,
            last_keyframe_request: HashMap::new(),
            order: Vec::new(),
            states: HashMap::new(),
            notifiers: HashMap::new(),
            debt_ticks: 0,
        }
    }

    fn register(&mut self, mid: Mid, rid: Option<Rid>, notifier: KeyframeNotifier) {
        let key = (mid, rid);
        // q (rank 0) and non-simulcast tracks start active immediately.
        // h and f start paused; debt-free ticks resume them once BWE confirms budget.
        let paused = rid.is_some() && rid_quality_rank(rid) > 0;
        self.order.push(key);
        // Sort so index 0 = highest priority (q), last = lowest priority (f).
        self.order.sort_by_key(|(_, rid)| rid_quality_rank(*rid));
        // Seed the bitrate estimator so the upgrade gate sees a realistic cost
        // before the first 500 ms window of real measurements completes.
        self.states.insert(
            key,
            LayerState {
                bps: layer_seed_bps(rid),
                paused,
                estimate: BitrateEstimate::new_with_seed(layer_seed_bps(rid)),
            },
        );
        self.notifiers.insert(key, notifier);
    }

    fn update_available(&mut self, bw: Bitrate) {
        self.available_bps = bw.as_f64();
    }

    /// Called for every incoming frame regardless of whether the layer is currently paused.
    /// This gives us a true picture of what the encoder is producing.
    fn record_frame(&mut self, mid: Mid, rid: Option<Rid>, byte_len: usize, now: Instant) {
        let key = (mid, rid);
        let Some(s) = self.states.get_mut(&key) else {
            return;
        };
        s.estimate.record_bytes(byte_len, now);
    }

    fn refresh_bitrate_estimates(&mut self, now: Instant) {
        for s in self.states.values_mut() {
            s.estimate.poll(now);
            s.bps = s.estimate.estimate_bps();
        }
    }

    fn is_paused(&self, mid: Mid, rid: Option<Rid>) -> bool {
        self.states.get(&(mid, rid)).is_some_and(|s| s.paused)
    }

    fn request_keyframe(&mut self, mid: Mid, rid: Option<Rid>, kind: KeyframeRequestKind) {
        // Throttle PLI requests to avoid encoder storm like libwebrtc.
        if kind == KeyframeRequestKind::Pli {
            let key = (mid, rid);
            let now = Instant::now();
            if let Some(last) = self.last_keyframe_request.get(&key)
                && now.duration_since(*last) < KEYFRAME_REQUEST_THROTTLE
            {
                tracing::debug!(mid = ?mid, rid = ?rid, "throttling repeated PLI");
                return;
            }
            self.last_keyframe_request.insert(key, now);
        }

        if let Some(n) = self.notifiers.get(&(mid, rid)) {
            n.notify();
        }
    }

    fn tick(&mut self, now: Instant) -> f64 {
        if self.order.is_empty() {
            return 0.0;
        }

        // Advance per-layer bps estimates from frames accumulated since last tick.
        self.refresh_bitrate_estimates(now);

        for k in &self.order {
            if let Some(s) = self.states.get(k) {
                tracing::debug!(
                    rid = ?k.1,
                    bps = %Bitrate::from(s.bps),
                    paused = s.paused,
                    available = %Bitrate::from(self.available_bps),
                    "bwe: layer state"
                );
            }
        }

        // Desired = sum of ALL layers regardless of paused state.
        // This keeps BWE probing toward the full simulcast bandwidth.
        let desired: f64 = self
            .order
            .iter()
            .filter_map(|k| self.states.get(k))
            .map(|s| s.bps)
            .sum();

        // No BWE estimate yet — return the desired target but don't touch layers.
        if self.available_bps == f64::MAX {
            return desired;
        }

        let allocated: f64 = self
            .order
            .iter()
            .filter_map(|k| self.states.get(k))
            .filter(|s| !s.paused)
            .map(|s| s.bps)
            .sum();

        // Debt = how far active layers exceed available bandwidth (negative = surplus).
        let debt = allocated - self.available_bps;

        // Track consecutive ticks where debt is non-trivial.
        if debt > self.available_bps * DEBT_LINGER_THRESHOLD {
            self.debt_ticks += 1;
        } else {
            self.debt_ticks = 0;
        }

        let must_shed = debt > self.available_bps * DEBT_IMMEDIATE_THRESHOLD
            || self.debt_ticks >= DEBT_LINGER_MAX_TICKS;

        if must_shed {
            // Shed the lowest-priority active simulcast layer (f first, then h, then q).
            if let Some(key) = self
                .order
                .iter()
                .rev()
                .find(|k| self.states.get(*k).is_some_and(|s| !s.paused) && k.1.is_some())
                .cloned()
            {
                if let Some(s) = self.states.get_mut(&key) {
                    s.paused = true;
                }
                self.debt_ticks = 0;
                tracing::info!(
                    mid = ?key.0,
                    rid = ?key.1,
                    debt = %Bitrate::from(debt.max(0.0)),
                    available = %Bitrate::from(self.available_bps),
                    "bwe: pause layer"
                );
            }
        } else if debt <= 0.0 {
            // No debt — try to resume the highest-priority paused layer if it fits.
            // order is q→h→f so the first paused entry is the most important to restore.
            if let Some(key) = self
                .order
                .iter()
                .find(|k| self.states.get(*k).is_some_and(|s| s.paused))
                .cloned()
            {
                let candidate_bps = self.states.get(&key).map_or(0.0, |s| s.bps);
                let surplus = -debt; // available - allocated
                if candidate_bps > 0.0 && candidate_bps <= surplus {
                    if let Some(s) = self.states.get_mut(&key) {
                        s.paused = false;
                    }
                    tracing::info!(
                        mid = ?key.0,
                        rid = ?key.1,
                        available = %Bitrate::from(self.available_bps),
                        candidate = %Bitrate::from(candidate_bps),
                        surplus = %Bitrate::from(surplus),
                        "bwe: resume layer"
                    );
                    if let Some(n) = self.notifiers.get(&key) {
                        n.notify();
                    }
                }
            }
        }
        // else: debt is small but positive — acceptable, do nothing this tick.

        desired
    }
}

pub type TrackId = String;
pub type ParticipantId = String;

#[derive(Debug, Default, Clone)]
pub struct AgentStats {
    pub peer: Option<str0m::stats::PeerStats>,
    pub tracks: HashMap<Mid, TrackStats>,
}

impl AgentStats {
    pub fn total_rx_bytes(&self) -> u64 {
        self.tracks
            .values()
            .flat_map(|t| t.rx_layers.values())
            .map(|s| s.bytes)
            .sum()
    }

    pub fn total_tx_bytes(&self) -> u64 {
        self.tracks
            .values()
            .flat_map(|t| t.tx_layers.values())
            .map(|s| s.bytes)
            .sum()
    }
}

#[derive(Debug, Default, Clone)]
pub struct TrackStats {
    pub kind: Option<MediaKind>,
    pub rx_layers: HashMap<Option<Rid>, str0m::stats::MediaIngressStats>,
    pub tx_layers: HashMap<Option<Rid>, str0m::stats::MediaEgressStats>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoPreset {
    Camera,
    Screen,
}

impl VideoPreset {
    pub fn base_bitrate(&self) -> u64 {
        match self {
            Self::Camera => 1_250_000,
            Self::Screen => 2_500_000,
        }
    }

    pub fn content_hint(&self) -> &str {
        match self {
            Self::Camera => "motion",
            Self::Screen => "text",
        }
    }
}

#[derive(Debug)]
pub struct LocalTrack {
    pub kind: MediaKind,
    pub mid: Mid,
    pub rid: Option<Rid>,
    pub(crate) tx: mpsc::Sender<MediaFrame>,
    pub keyframe_rx: KeyframeReceiver,
}

impl LocalTrack {
    pub fn try_send(&self, frame: MediaFrame) {
        let _ = self.tx.try_send(frame);
    }

    pub async fn send(&self, frame: MediaFrame) {
        let _ = self.tx.send(frame).await;
    }
}

#[derive(thiserror::Error, Debug)]
pub enum AgentError {
    #[error("API call failed: {0}")]
    Api(#[from] ApiError),
    #[error("RTC Error: {0}")]
    Rtc(#[from] str0m::RtcError),
    #[error("IO Error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Protocol Error: {0}")]
    Protocol(String),
    #[error("No valid network candidates found")]
    NoCandidates,
}

#[derive(Debug, Clone)]
struct TrackRequest {
    kind: MediaKind,
    direction: TransceiverDirection,
    simulcast_layers: Option<Vec<SimulcastLayer>>,
}

pub struct AgentBuilder {
    api: HttpApiClient,
    udp_socket: UdpSocket,
    tracks: Vec<TrackRequest>,
    local_ips: Vec<IpAddr>,
    /// If set, the agent eagerly opens a TCP connection to this address and
    /// adds a TCP active ICE candidate so that the server's TCP passive
    /// candidate forms a valid check pair.
    tcp_server_addr: Option<SocketAddr>,
}

impl AgentBuilder {
    pub fn new(api: HttpApiClient, udp_socket: UdpSocket) -> AgentBuilder {
        Self {
            api,
            udp_socket,
            tracks: Vec::new(),
            local_ips: Vec::new(),
            tcp_server_addr: None,
        }
    }

    pub fn with_track(
        mut self,
        kind: MediaKind,
        direction: TransceiverDirection,
        simulcast_layers: Option<Vec<SimulcastLayer>>,
    ) -> Self {
        self.tracks.push(TrackRequest {
            kind,
            direction,
            simulcast_layers,
        });
        self
    }

    pub fn with_local_ip(mut self, ip: IpAddr) -> Self {
        self.local_ips.push(ip);
        self
    }

    /// Connect a TCP stream to `addr` before SDP negotiation so that the agent
    /// can advertise a TCP active candidate and route TCP traffic through it.
    pub fn with_tcp_server_addr(mut self, addr: SocketAddr) -> Self {
        self.tcp_server_addr = Some(addr);
        self
    }

    pub async fn connect(mut self, room_id: &str) -> Result<AgentDriver, AgentError> {
        let port = self.udp_socket.local_addr()?.port();

        if self.local_ips.is_empty() {
            self.local_ips.extend(
                if_addrs::get_if_addrs()?
                    .into_iter()
                    .filter(|i| !i.is_loopback())
                    .map(|i| i.ip()),
            )
        }

        tracing::info!("local ips: {:?}", self.local_ips);

        let mut rtc_builder = Rtc::builder()
            .clear_codecs()
            .enable_bwe(Some(Bitrate::kbps(500)))
            .set_extension(
                rtp_extensions::ABS_CAPTURE_TIME,
                Extension::AbsoluteCaptureTime,
            )
            .set_stats_interval(Some(Duration::from_millis(200)));
        let codec_config = rtc_builder.codec_config();
        codec_config.enable_opus(true);

        let baseline_levels = [0x34]; // 5.2 level matching OpenH264
        let mut pt = 96; // start around 96–127 range for dynamic types

        for level in &baseline_levels {
            // Constrained Baseline
            codec_config.add_h264(
                pt.into(),
                Some((pt + 1).into()), // RTX PT
                true,
                0x42e000 | level,
            );
            pt += 2;
        }

        let mut rtc = rtc_builder.build(Instant::now().into());
        let mut candidate_count = 0;
        let mut maybe_addr = None;
        for ip in &self.local_ips {
            let addr = SocketAddr::new(*ip, port);
            let candidate = match Candidate::builder().udp().host(addr).build() {
                Ok(candidate) => candidate,
                Err(err) => {
                    tracing::warn!("ignored bad candidate: {:?}", err);
                    continue;
                }
            };
            rtc.add_local_candidate(candidate);
            maybe_addr = Some(addr);
            candidate_count += 1;
        }

        // If a server TCP address is provided, eagerly connect and add a TCP
        // active local candidate per RFC 6544 (uses port 9 as placeholder).
        let mut tcp_stream: Option<pulsebeam_core::net::TcpStream> = None;
        let mut tcp_local_addr: Option<SocketAddr> = None;
        let mut tcp_server_addr: Option<SocketAddr> = None;
        if let Some(server_tcp) = self.tcp_server_addr {
            match pulsebeam_core::net::TcpStream::connect(server_tcp).await {
                Ok(stream) => {
                    let _ = stream.set_nodelay(true);
                    let local = stream.local_addr().ok();
                    tcp_local_addr = local;
                    tcp_server_addr = Some(server_tcp);
                    tcp_stream = Some(stream);

                    // Add TCP active local candidate for each configured local IP.
                    // Port 9 is the discard port used as a placeholder for the
                    // active side per RFC 6544 §4.5.
                    for ip in &self.local_ips {
                        let tcp_candidate_addr = SocketAddr::new(*ip, 9);
                        match Candidate::builder()
                            .tcp()
                            .host(tcp_candidate_addr)
                            .tcptype(TcpType::Active)
                            .build()
                        {
                            Ok(c) => {
                                rtc.add_local_candidate(c);
                                candidate_count += 1;
                                if maybe_addr.is_none() {
                                    maybe_addr = Some(tcp_candidate_addr);
                                }
                            }
                            Err(err) => {
                                tracing::warn!("ignored bad TCP candidate: {:?}", err);
                            }
                        }
                    }
                    tracing::info!(?server_tcp, "TCP stream connected to server");
                }
                Err(e) => {
                    return Err(AgentError::Io(e));
                }
            }
        }

        if candidate_count == 0 {
            return Err(AgentError::NoCandidates);
        }

        // TODO: map multiple addresses?
        let Some(addr) = maybe_addr else {
            return Err(AgentError::NoCandidates);
        };

        let signaling_cid = rtc.direct_api().create_data_channel(ChannelConfig {
            label: namespace::Signaling::Reliable.as_str().to_string(),
            ordered: true,
            reliability: Reliability::Reliable,
            negotiated: Some(0),
            protocol: "v1".to_string(),
        });

        let mut sdp = rtc.sdp_api();
        // sdp.add_channel("sctp-enable".to_string());
        let mut medias = Vec::new();
        for track in self.tracks.clone() {
            let (dir, simulcast) = match track.direction {
                TransceiverDirection::SendOnly => (
                    Direction::SendOnly,
                    track.simulcast_layers.map(|layers| Simulcast {
                        send: layers,
                        recv: Vec::new(),
                    }),
                ),
                TransceiverDirection::RecvOnly => (
                    Direction::RecvOnly,
                    track.simulcast_layers.map(|layers| Simulcast {
                        send: Vec::new(),
                        recv: layers,
                    }),
                ),
            };
            let mid = sdp.add_media(track.kind, dir, None, None, simulcast.clone());
            // TODO: why do we need to emit manually here?
            medias.push(MediaAdded {
                mid,
                kind: track.kind,
                direction: dir,
                simulcast,
            });
        }
        let (offer, pending) = sdp
            .apply()
            .ok_or_else(|| AgentError::Protocol("SDP apply produced no offer".into()))?;
        tracing::debug!("Generated SDP Offer:\n{}", offer);
        let resp = self
            .api
            .create_participant(CreateParticipantRequest {
                room_id: room_id.to_string(),
                offer,
            })
            .await?;

        rtc.sdp_api()
            .accept_answer(pending, resp.answer)
            .map_err(AgentError::Rtc)?;

        let mut driver = AgentDriver {
            api: self.api,
            addr,
            rtc,
            now: Instant::now(),
            stats: AgentStats::default(),
            socket: self.udp_socket,
            buf: vec![0u8; 2048],
            tcp: match tcp_stream {
                Some(s) => TcpSession::new(s, tcp_local_addr, tcp_server_addr.unwrap()),
                None => TcpSession::inactive(),
            },
            pending_events: VecDeque::new(),
            resource_uri: resp.resource_uri,
            participant_id: resp.participant_id.clone(),
            senders: StreamMap::new(),
            pending: PendingState::new(),
            slot_manager: SlotManager::new(),
            disconnected_reason: None,
            signaling_cid,
            layer_ctrl: LayerController::new(),
            desired_ctrl: BitrateControllerConfig::default().build(),
            sub_manager: SubscriptionManager::new(
                medias
                    .iter()
                    .filter(|m| m.direction == Direction::RecvOnly)
                    .map(|m| m.mid)
                    .collect(),
            ),
            preset: VideoPreset::Camera,
            retry_count: 0,
            is_reconnecting: false,
            reconnect_deadline: None,
            notifier: tokio::sync::Notify::new(),
            sleep: Box::pin(tokio::time::sleep(MIN_QUANTA)),
            bwe_slow_timer: tokio::time::interval(BWE_SLOW_INTERVAL),
            debounce_timer: Box::pin(tokio::time::sleep(Duration::ZERO)),
            reconnect_timer: Box::pin(tokio::time::sleep(Duration::ZERO)),
        };

        for media in medias {
            driver.handle_media_added(media);
        }

        Ok(driver)
    }
}

#[derive(Debug)]
pub enum AgentEvent {
    LocalTrackAdded(LocalTrack),
    /// The server announced a remote track (metadata only) even if the client
    /// has not yet subscribed to it.
    RemoteTrackDiscovered(pulsebeam_proto::signaling::Track),
    /// A remote track has been assigned to a receive slot (MID).
    RemoteTrackAdded {
        mid: Mid,
        track: Arc<Track>,
    },
    /// A media frame was received from a remote track.
    MediaReceived {
        mid: Mid,
        track: Arc<Track>,
        frame: MediaFrame,
        receive_time: Instant,
    },
    Connected,
    Disconnected(String),
}

pub struct AgentDriver {
    addr: SocketAddr,
    rtc: Rtc,
    socket: UdpSocket,
    buf: Vec<u8>,
    /// Active TCP connection to the server (RFC 6544 client role).
    tcp: TcpSession,
    stats: AgentStats,
    pending_events: VecDeque<AgentEvent>,

    senders: StreamMap<(Mid, Option<Rid>), ReceiverStream<MediaFrame>>,
    pending: PendingState,
    slot_manager: SlotManager,

    api: HttpApiClient,
    signaling_cid: ChannelId,
    resource_uri: Uri,
    participant_id: String,
    disconnected_reason: Option<String>,

    /// Egress-side BWE layer controller: pauses/resumes simulcast layers
    /// based on TWCC bandwidth estimates.
    layer_ctrl: LayerController,
    desired_ctrl: BitrateController,

    sub_manager: SubscriptionManager,
    preset: VideoPreset,

    retry_count: u32,
    is_reconnecting: bool,
    reconnect_deadline: Option<Instant>,

    // Timer state
    now: Instant,
    notifier: tokio::sync::Notify,
    sleep: Pin<Box<tokio::time::Sleep>>,
    bwe_slow_timer: tokio::time::Interval,
    debounce_timer: Pin<Box<tokio::time::Sleep>>,
    reconnect_timer: Pin<Box<tokio::time::Sleep>>,
}

impl AgentDriver {
    pub fn stats(&self) -> &AgentStats {
        &self.stats
    }

    pub fn participant_id(&self) -> &ParticipantId {
        &self.participant_id
    }

    /// Perform cleanup after the event loop ends.
    /// Called automatically by `run()`. If you drive `poll()` manually, call
    /// this once `poll()` returns `None`.
    pub async fn shutdown(&mut self) {
        if let Err(e) = self
            .api
            .delete_participant_by_uri(self.resource_uri.clone())
            .await
        {
            tracing::warn!(error = ?e, "failed to delete participant on shutdown");
        }
        self.rtc.disconnect();
        self.notifier.notify_one();
    }

    pub fn set_subscriptions(&mut self, subs: Vec<Subscription>) {
        tracing::debug!("processing SetSubscriptions: {:?}", subs);
        self.sub_manager.set_desired(subs);
        self.pending.deadline.replace(self.now + STATE_DEBOUNCE);
        // Make sure we attempt to send the new desired state immediately.
        self.flush_pending_state();
        self.notifier.notify_one();
    }

    pub fn set_preset(&mut self, preset: VideoPreset) {
        self.preset = preset;
        // In a more complete implementation, we would update RTCPeerConnection parameters here
        // if the underlying WebRTC implementation supports it.
        self.notifier.notify_one();
    }

    /// Drive one iteration of the agent event loop.
    /// Returns the next available event, or `None` when the agent has shut down.
    ///
    /// Designed to be used in a `tokio::select!` alongside other futures, or
    /// in a plain `while let Some(event) = driver.poll().await` loop.
    pub async fn poll(&mut self) -> Option<AgentEvent> {
        // Return events queued by the previous iteration first.
        if let Some(ev) = self.pending_events.pop_front() {
            return Some(ev);
        }

        loop {
            // Drain all str0m output — this may queue pending_events.
            let Some(deadline) = self.poll_rtc().await else {
                return self.pending_events.pop_front();
            };

            // Return events produced by poll_rtc (MediaData, Connected, etc.).
            if let Some(ev) = self.pending_events.pop_front() {
                return Some(ev);
            }

            // Adjust timers.
            self.now = Instant::now();
            let adjusted_deadline = if deadline <= self.now {
                self.now + MIN_QUANTA
            } else {
                deadline
            };
            if self.sleep.deadline() != adjusted_deadline {
                self.sleep.as_mut().reset(adjusted_deadline);
            }
            if let Some(reconnect_at) = self.reconnect_deadline
                && self.reconnect_timer.deadline() != reconnect_at
            {
                self.reconnect_timer.as_mut().reset(reconnect_at);
            }
            if let Some(debounce_at) = self.pending.deadline
                && self.debounce_timer.deadline() != debounce_at
            {
                self.debounce_timer.as_mut().reset(debounce_at);
            }

            // Wait for the next IO/timer event, then loop back to poll_rtc().
            tokio::select! {
                biased;
                _ = self.notifier.notified() => {}
                // If we have a pending state update (for subscription changes),
                // flush it when the debounce timer fires.
                _ = self.debounce_timer.as_mut(), if self.pending.deadline.is_some() => {
                    self.flush_pending_state();
                }
                res = self.socket.recv_from(&mut self.buf) => {
                    if let Ok((n, source)) = res {
                        match self.buf[..n].try_into() {
                            Ok(contents) => {
                                let _ = self.rtc.handle_input(Input::Receive(
                                    Instant::now().into(),
                                    Receive {
                                        proto: Protocol::Udp,
                                        source,
                                        destination: self.addr,
                                        contents,
                                    }
                                ));
                            }
                            Err(_) => {
                                tracing::warn!(n, "UDP datagram too large for RTC buffer, discarding");
                            }
                        }
                    }
                }
                // TCP receive path: decode RFC 4571 frames and feed to str0m.
                res = self.tcp.wait_recv() => {
                    self.tcp.on_recv(res, &mut self.rtc);
                }
                // Drain send frames from looper tasks.
                // Re-stamp abs_capture_time at wire-send time for accurate
                // end-to-end latency measurement (excludes send-queue depth).
                Some(((mid, rid), frame)) = self.senders.next() => {
                    let paused = self.layer_ctrl.is_paused(mid, rid);
                    // Measure encoder output bitrate before the pause gate so
                    // BWE always sees the true encoder cost even when suppressed.
                    self.layer_ctrl.record_frame(mid, rid, frame.data.len(), Instant::now());

                    if !paused {
                        if let Some(mut writer) = self.rtc.writer(mid) {
                            let Some(pt) = writer.payload_params().next().map(|p| p.pt()) else {
                                tracing::warn!(?mid, "no payload params found for writer");
                                continue;
                            };
                            if let Some(rid) = rid {
                                writer = writer.rid(rid);
                            }
                            // Re-stamp at wire-send time (not encode time) so that
                            // receive_time - abs_capture_time measures network +
                            // server forwarding latency only.
                            writer = writer.abs_capture_time(AbsCaptureTime {
                                capture_time: crate::clock::capture_wallclock(),
                                clock_offset: None,
                            });
                            let _ = writer.write(pt, frame.capture_time.into(), frame.ts, frame.data);
                        } else {
                            tracing::warn!(?mid, "no writer found for mid");
                        }
                    }
                }
                _ = self.bwe_slow_timer.tick() => {
                    let desired_bps = self.layer_ctrl.tick(self.now);
                    let desired_bitrate = Bitrate::from(desired_bps.max(0.0) as u64);
                    let filtered_bitrate = self.desired_ctrl.update(desired_bitrate);
                    self.rtc.bwe().set_desired_bitrate(filtered_bitrate);
                }
                _ = self.sleep.as_mut() => {
                    if self.rtc.handle_input(Input::Timeout(Instant::now().into())).is_err() {
                        self.emit(AgentEvent::Disconnected("RTC Timeout".into()));
                    }
                }
                _ = self.reconnect_timer.as_mut(), if self.reconnect_deadline.is_some() => {
                    self.perform_reconnect().await;
                }
            }

            // Return any events produced by this select iteration.
            if let Some(ev) = self.pending_events.pop_front() {
                return Some(ev);
            }
            // No event yet — loop back to drain str0m output.
        }
    }

    /// Drive the agent to completion, silently dropping events.
    /// For fire-and-forget embeddings. Call this via `tokio::spawn(driver.run())`.
    /// For event-driven use, call `poll()` in your own loop instead.
    pub async fn run(mut self) {
        while self.poll().await.is_some() {}
        self.shutdown().await;
    }

    async fn poll_rtc(&mut self) -> Option<Instant> {
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Transmit(tx)) => match tx.proto {
                    Protocol::Udp => {
                        let _ = self.socket.send_to(&tx.contents, tx.destination).await;
                    }
                    Protocol::Tcp => {
                        self.tcp.send(&tx.contents).await;
                    }
                    _ => {}
                },
                Ok(Output::Event(e)) => match e {
                    Event::ChannelOpen(cid, label) => {
                        tracing::info!(
                            "channel open: CID={:?}, Label={:?}, Target Signaling CID={:?}",
                            cid,
                            label,
                            self.signaling_cid
                        );
                        if label == namespace::Signaling::Reliable.as_str() {
                            tracing::info!("signaling channel is now open, CID={:?}", cid);
                            self.signaling_cid = cid;
                            // Reset stale active assignments so that the
                            // reconciler re-sends all desired subscriptions to
                            // the freshly-created server-side participant.
                            self.sub_manager.reset_active_assignments();
                            self.pending.deadline.replace(Instant::now());
                        }
                    }
                    Event::ChannelData(data) => {
                        self.handle_signaling_data(data);
                    }
                    Event::MediaAdded(media) => self.handle_media_added(media),

                    Event::MediaData(data) => {
                        let receive_time = Instant::now();
                        if let Some(track) = self.slot_manager.get_active_track(&data.mid) {
                            let track = track.clone();
                            let mid = data.mid;
                            self.emit(AgentEvent::MediaReceived {
                                mid,
                                track,
                                frame: data.into(),
                                receive_time,
                            });
                        }
                    }

                    Event::IceConnectionStateChange(state) => {
                        tracing::info!("connection state changed: {:?}", state);
                        if state == IceConnectionState::Disconnected {
                            self.schedule_reconnect(Instant::now());
                        }
                    }
                    Event::Connected => {
                        self.emit(AgentEvent::Connected);
                    }
                    Event::PeerStats(stats) => {
                        self.stats.peer = Some(stats);
                    }
                    Event::MediaIngressStats(stats) => {
                        let track_stats = self.stats.tracks.entry(stats.mid).or_default();
                        track_stats.rx_layers.insert(stats.rid, stats);
                    }
                    Event::MediaEgressStats(stats) => {
                        let track_stats = self.stats.tracks.entry(stats.mid).or_default();
                        track_stats.tx_layers.insert(stats.rid, stats);
                    }
                    Event::KeyframeRequest(req) => {
                        tracing::debug!(mid = ?req.mid, rid = ?req.rid, kind = ?req.kind, "keyframe request");
                        self.layer_ctrl.request_keyframe(req.mid, req.rid, req.kind);
                    }
                    Event::EgressBitrateEstimate(BweKind::Twcc(available)) => {
                        self.layer_ctrl.update_available(available);
                    }
                    e => {
                        tracing::trace!("unhandled event: {:?}", e);
                    }
                },
                Ok(Output::Timeout(t)) => {
                    return Some(t.into());
                }
                Err(e) => {
                    tracing::error!("RTC Error: {:?}", e);
                    self.disconnected_reason = Some(format!("RTC Error: {:?}", e));
                    self.rtc.disconnect();
                    return None;
                }
            }
        }
    }

    fn handle_media_added(&mut self, media: MediaAdded) {
        let mid = media.mid;
        tracing::info!("new media added: {:?}", media);
        // Record the kind so the CLI can distinguish video from audio streams.
        self.stats.tracks.entry(mid).or_default().kind = Some(media.kind);
        match media.direction {
            Direction::SendOnly => {
                let rids = if let Some(layers) = media.simulcast {
                    layers.send.iter().map(|s| Some(s.rid)).collect()
                } else {
                    vec![None]
                };

                for rid in rids {
                    // User wants to send. We create a channel: User(tx) -> Actor(rx)
                    let (tx, rx) = mpsc::channel(8);
                    // Create a keyframe notification channel: Actor(notifier) -> Looper(rx).
                    // The actor signals the looper to seek to the nearest IDR whenever
                    // BWE un-pauses this layer.
                    let (kf_notifier, kf_rx) = KeyframeNotifier::pair();
                    if media.kind.is_video() {
                        self.layer_ctrl.register(mid, rid, kf_notifier);
                    }
                    self.senders.insert((mid, rid), ReceiverStream::new(rx));

                    self.emit(AgentEvent::LocalTrackAdded(LocalTrack {
                        kind: media.kind,
                        mid,
                        tx,
                        rid,
                        keyframe_rx: kf_rx,
                    }));
                }
            }
            Direction::RecvOnly => {
                // Remote wants to send. We create a channel: Actor(tx) -> User(rx)
                self.slot_manager.register(mid);
            }
            dir => {
                tracing::warn!("{} transceiver direction is not supported", dir);
            }
        }
    }

    fn handle_signaling_data(&mut self, cd: ChannelData) {
        let Ok(msg) = signaling::ServerMessage::decode(cd.data.as_slice()) else {
            tracing::warn!("Invalid Protobuf");
            return;
        };

        let Some(payload) = msg.payload else {
            tracing::warn!("empty protobuf");
            return;
        };

        match payload {
            signaling::server_message::Payload::Update(update) => {
                tracing::info!("Received signaling update: {:?}", update);
                let (new_assignments, new_discovered_tracks) = self.slot_manager.sync(update);
                for track in new_discovered_tracks {
                    self.emit(AgentEvent::RemoteTrackDiscovered(track));
                }
                for (mid, track) in new_assignments {
                    tracing::info!("Emitting RemoteTrackAdded for MID: {:?}", mid);
                    self.emit(AgentEvent::RemoteTrackAdded { mid, track });
                }
            }
            signaling::server_message::Payload::Error(err) => {
                tracing::warn!("signaling error: {}", err);
            }
        }
    }

    fn emit(&mut self, event: AgentEvent) {
        self.pending_events.push_back(event);
    }

    fn flush_pending_state(&mut self) {
        let channel_exists = self.rtc.channel(self.signaling_cid).is_some();
        tracing::debug!(
            "flush_pending_state: signaling_cid={:?}, exists={}",
            self.signaling_cid,
            channel_exists
        );

        let Some(mut ch) = self.rtc.channel(self.signaling_cid) else {
            tracing::debug!("signaling channel not ready yet, will retry later");
            // Keep retrying in the future
            self.pending.deadline.replace(self.now + STATE_DEBOUNCE);
            return;
        };

        let requests = self.sub_manager.reconcile();
        tracing::debug!(
            "subscription reconcile produced {} requests",
            requests.len()
        );
        if requests.is_empty() {
            tracing::debug!("no subscription requests to send");
            self.pending.deadline = None;
            return;
        }

        tracing::debug!("sending subscription requests: {:?}", requests);
        let msg = signaling::ClientMessage {
            payload: Some(signaling::client_message::Payload::Intent(
                signaling::ClientIntent {
                    upstream_intents: vec![],
                    downstream_requests: requests,
                },
            )),
        };
        let encoded = msg.encode_to_vec();
        if let Err(err) = ch.write(true, encoded.as_slice()) {
            tracing::warn!("failed to send signaling: {:?}", err);
            // Retry later
            self.pending.deadline.replace(self.now + STATE_DEBOUNCE);
        } else {
            self.pending.deadline = None;
        }
    }

    fn schedule_reconnect(&mut self, now: Instant) {
        if self.is_reconnecting {
            return;
        }

        let delay = match self.retry_count {
            0 => Duration::ZERO,
            1 => Duration::from_millis(500),
            n => Duration::from_millis(500 * 2u64.pow(n.min(10) - 1)).min(Duration::from_secs(5)),
        };

        self.retry_count += 1;
        self.reconnect_deadline = Some(now + delay);
        tracing::info!(
            "scheduling reconnect in {:?} (attempt {})",
            delay,
            self.retry_count
        );
    }

    async fn perform_reconnect(&mut self) {
        self.is_reconnecting = true;
        self.reconnect_deadline = None;
        // Clear stale peer stats so callers don't read pre-reconnect RTT.
        self.stats.peer = None;

        tracing::info!("performing reconnect (attempt {})", self.retry_count);

        match self.try_reconnect().await {
            Ok(_) => {
                tracing::info!("reconnect successful");
                self.is_reconnecting = false;
                self.retry_count = 0;
                self.emit(AgentEvent::Connected);
            }
            Err(err) => {
                tracing::warn!("reconnect failed: {:?}", err);
                self.is_reconnecting = false;
                self.schedule_reconnect(Instant::now());
            }
        }
    }

    async fn try_reconnect(&mut self) -> Result<(), AgentError> {
        let (offer, pending) = {
            let sdp_api = self.rtc.sdp_api();
            match sdp_api.apply() {
                Some(pair) => pair,
                None => {
                    tracing::info!(
                        "Nothing to negotiate during reconnect; assuming connection is already up"
                    );
                    return Ok(());
                }
            }
        };

        let resp = self
            .api
            .update_participant(
                self.resource_uri.clone(),
                crate::api::UpdateParticipantRequest { offer },
            )
            .await?;

        self.rtc
            .sdp_api()
            .accept_answer(pending, resp.answer)
            .map_err(AgentError::Rtc)?;

        Ok(())
    }
}

struct PendingState {
    deadline: Option<Instant>,
    requests: Vec<pulsebeam_proto::signaling::VideoRequest>,
}

impl PendingState {
    fn new() -> Self {
        Self {
            requests: Vec::new(),
            deadline: None,
        }
    }

    /// Adds or updates a request in the pending list
    fn upsert(&mut self, mid: Mid, track_id: String, height: u32) {
        if let Some(req) = self
            .requests
            .iter_mut()
            .find(|r| r.mid.as_bytes() == mid.as_bytes())
        {
            req.track_id = track_id;
            req.height = height;
        } else {
            self.requests
                .push(pulsebeam_proto::signaling::VideoRequest {
                    mid: mid.to_string(),
                    track_id,
                    height,
                });
        }
    }

    /// Assigns a track to a slot (finding the slot happens outside or via helper)
    fn assign(&mut self, slots: &SlotManager, track_id: String, height: u32) {
        if let Some(mid) = self.find_mid_for_track(slots, &track_id) {
            self.upsert(mid, track_id, height);
            return;
        }

        if let Some(mid) = self.find_free_mid(slots) {
            self.upsert(mid, track_id, height);
        } else {
            tracing::warn!("No free receiver slots available for track: {}", track_id);
        }
    }

    /// Stops receiving a track
    fn unassign(&mut self, slots: &SlotManager, track_id: String) {
        if let Some(mid) = self.find_mid_for_track(slots, &track_id) {
            // Send height: 0 to stop
            self.upsert(mid, track_id, 0);
        }
    }

    /// Returns the track_id effectively assigned to a MID.
    /// Looks at Pending first, falls back to SlotManager.
    fn get_effective_track<'a>(&'a self, slots: &'a SlotManager, mid: &str) -> Option<&'a str> {
        if let Some(req) = self.requests.iter().find(|r| r.mid == mid) {
            if req.height > 0 {
                return Some(&req.track_id);
            } else {
                // Explicitly stopped in pending
                return None;
            }
        }

        slots.get_track_for_mid(mid)
    }

    fn find_mid_for_track(&self, slots: &SlotManager, track_id: &str) -> Option<Mid> {
        for slot in &slots.slots {
            if self.get_effective_track(slots, &slot.mid) == Some(track_id) {
                return Some(slot.mid);
            }
        }
        None
    }

    fn find_free_mid(&self, slots: &SlotManager) -> Option<Mid> {
        for slot in &slots.slots {
            if self.get_effective_track(slots, &slot.mid).is_none() {
                return Some(slot.mid);
            }
        }
        None
    }

    fn is_dirty(&self) -> bool {
        !self.requests.is_empty()
    }

    fn take(&mut self) -> Vec<pulsebeam_proto::signaling::VideoRequest> {
        self.deadline.take();
        self.requests.drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;
    use str0m::media::{KeyframeRequestKind, Mid, Rid};

    #[test]
    fn throttles_pli_keyframe_requests() {
        let mut ctrl = LayerController::new();
        let mid = Mid::from("mid0");
        let rid = Some(Rid::from("q"));
        let (notifier, mut receiver) = KeyframeNotifier::pair();

        ctrl.register(mid, rid, notifier);

        ctrl.request_keyframe(mid, rid, KeyframeRequestKind::Pli);
        assert!(receiver.is_requested());

        ctrl.request_keyframe(mid, rid, KeyframeRequestKind::Pli);
        assert!(!receiver.is_requested());

        sleep(Duration::from_millis(1100));
        ctrl.request_keyframe(mid, rid, KeyframeRequestKind::Pli);
        assert!(receiver.is_requested());
    }

    #[test]
    fn fir_is_not_throttled() {
        let mut ctrl = LayerController::new();
        let mid = Mid::from("mid1");
        let rid = Some(Rid::from("h"));
        let (notifier, mut receiver) = KeyframeNotifier::pair();

        ctrl.register(mid, rid, notifier);

        ctrl.request_keyframe(mid, rid, KeyframeRequestKind::Fir);
        assert!(receiver.is_requested());

        // Fire again quickly; should still be granted.
        ctrl.request_keyframe(mid, rid, KeyframeRequestKind::Fir);
        assert!(receiver.is_requested());
    }

    fn replay_frames_into_agent_bwe(frame_sizes: &[usize], fps: u64) -> Vec<f64> {
        let frame_interval = Duration::from_micros(1_000_000 / fps);
        let tick_dur = Duration::from_millis(500);
        let t0 = Instant::now();
        let mut estimator = BitrateEstimate::new_with_seed(0.0);
        let mut now = t0;
        let mut next_sample = t0 + tick_dur;
        let mut samples = Vec::new();

        for &frame_bytes in frame_sizes {
            now += frame_interval;
            estimator.record_bytes(frame_bytes, now);
            while now >= next_sample {
                estimator.poll(next_sample);
                samples.push(estimator.estimate_bps());
                next_sample += tick_dur;
            }
        }

        samples
    }

    #[test]
    fn bwe_quarter_h264_stable_cbr_estimate() {
        const RAW_MAX_KBPS: f64 = 225.86;
        const RAW_MIN_KBPS: f64 = 97.01;
        const RAW_RATIO: f64 = RAW_MAX_KBPS / RAW_MIN_KBPS;
        const TARGET_BPS: f64 = 150_000.0;

        let frames = pulsebeam_testdata::h264_frame_sizes(pulsebeam_testdata::RAW_H264_QUARTER_CBR);
        let samples = replay_frames_into_agent_bwe(&frames, 30);

        assert!(
            samples.len() >= 20,
            "expected ≥ 20 samples, got {}",
            samples.len()
        );
        let steady: &[f64] = &samples[10..];

        let mean = steady.iter().sum::<f64>() / steady.len() as f64;
        let max = steady.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let min = steady.iter().copied().fold(f64::INFINITY, f64::min);
        let ratio = max / min.max(1.0);

        assert!(
            mean >= TARGET_BPS * 1.0,
            "mean estimate too low: {:.0} bps (target {:.0})",
            mean,
            TARGET_BPS
        );
        assert!(
            mean <= TARGET_BPS * 1.15,
            "mean estimate too high: {:.0} bps (target {:.0})",
            mean,
            TARGET_BPS
        );
        assert!(
            ratio < RAW_RATIO,
            "estimate less stable than raw 1 s window: min={:.0} max={:.0} ratio={:.2}× > raw_ratio={:.2}×",
            min,
            max,
            ratio,
            RAW_RATIO
        );
    }

    #[test]
    fn pauses_and_resumes_layers_with_keyframe_request() {
        let mut ctrl = LayerController::new();
        let mid = Mid::from("mid2");
        let rid_q = Some(Rid::from("q"));
        let rid_h = Some(Rid::from("h"));
        let (notifier_q, mut receiver_q) = KeyframeNotifier::pair();
        let (notifier_h, mut receiver_h) = KeyframeNotifier::pair();

        ctrl.register(mid, rid_q, notifier_q);
        ctrl.register(mid, rid_h, notifier_h);

        assert!(!ctrl.is_paused(mid, rid_q));
        assert!(ctrl.is_paused(mid, rid_h));

        ctrl.update_available(Bitrate::kbps(10));
        let _ = ctrl.tick(Instant::now());
        assert!(ctrl.is_paused(mid, rid_q));

        ctrl.update_available(Bitrate::mbps(2));
        let _ = ctrl.tick(Instant::now());
        assert!(!ctrl.is_paused(mid, rid_q));
        assert!(receiver_q.is_requested() || receiver_h.is_requested());
    }
}

struct ReceiverSlot {
    mid: Mid,
    track_id: Option<TrackId>,
}

struct SlotManager {
    // Tracks discovered in the room but not yet assigned to a slot.
    pending_tracks: HashMap<TrackId, Arc<Track>>,
    // Active tracks (assigned to a slot and visible to the caller).
    active_tracks: HashMap<TrackId, Arc<Track>>,
    // The fixed set of Receive Transceivers (MIDs) available to this Agent.
    slots: Vec<ReceiverSlot>,
}

impl SlotManager {
    fn new() -> Self {
        Self {
            pending_tracks: HashMap::new(),
            slots: Vec::new(),
            active_tracks: HashMap::new(),
        }
    }

    /// Registers a permanent receiver slot (called during Agent init).
    fn register(&mut self, mid: Mid) {
        tracing::debug!(mid = ?mid, "Registering receiver slot");
        self.slots.push(ReceiverSlot {
            mid,
            track_id: None,
        });
    }

    fn get_track_for_mid(&self, mid: &str) -> Option<&str> {
        self.slots
            .iter()
            .find(|s| s.mid.as_bytes() == mid.as_bytes())
            .and_then(|s| s.track_id.as_deref())
    }

    fn sync(
        &mut self,
        update: pulsebeam_proto::signaling::StateUpdate,
    ) -> (
        Vec<(Mid, Arc<Track>)>,
        Vec<pulsebeam_proto::signaling::Track>,
    ) {
        tracing::info!("Syncing SlotManager state with update: {:?}", update);
        let mut new_assignments: Vec<(Mid, Arc<Track>)> = Vec::new();
        let mut newly_discovered_tracks = Vec::new();

        // 1. Handle track removals
        for t in update.tracks_remove {
            self.pending_tracks.remove(&t);
            self.active_tracks.remove(&t);
        }

        // 2. Handle new tracks (store as pending if not already active or pending)
        for t in update.tracks_upsert {
            if self.active_tracks.contains_key(&t.id) {
                // Already active, maybe update track metadata?
                // For now just keep it.
                continue;
            }
            if self.pending_tracks.contains_key(&t.id) {
                continue;
            }

            newly_discovered_tracks.push(t.clone());
            self.pending_tracks.insert(t.id.clone(), Arc::new(t));
        }

        // 3. Handle assignment removals
        for a in update.assignments_remove {
            if let Some(s) = self
                .slots
                .iter_mut()
                .find(|s| s.mid.as_bytes() == a.as_bytes())
            {
                s.track_id = None;
            }
        }

        // 4. Handle assignment updates
        for a in update.assignments_upsert {
            let Some(s) = self
                .slots
                .iter_mut()
                .find(|s| s.mid.as_bytes() == a.mid.as_bytes())
            else {
                continue;
            };

            if s.track_id.as_deref() == Some(&a.track_id) {
                // Already assigned to this track.
                continue;
            }

            s.track_id = Some(a.track_id.clone());

            // Try to promote the track if we already have its bitstream buffered.
            if let Some(track) = self.pending_tracks.remove(&a.track_id) {
                tracing::info!(
                    "Promoting track {} to active on MID {:?}",
                    a.track_id,
                    s.mid
                );
                let mid = s.mid;
                self.active_tracks.insert(a.track_id, track.clone());
                new_assignments.push((mid, track));
            } else {
                tracing::debug!(
                    "Track {} already active or not found in pending",
                    a.track_id
                );
            }
        }

        // 5. Handle cases where the assignment arrived before the track metadata.
        // If we already know about the track (in pending_tracks) and it is assigned,
        // promote it now.
        for slot in &self.slots {
            let Some(track_id) = &slot.track_id else {
                continue;
            };

            if self.active_tracks.contains_key(track_id) {
                continue;
            }

            let Some(track) = self.pending_tracks.remove(track_id) else {
                continue;
            };

            tracing::info!(
                "Promoting previously discovered track {} to active on MID {:?}",
                track_id,
                slot.mid
            );
            let mid = slot.mid;
            self.active_tracks.insert(track_id.clone(), track.clone());
            new_assignments.push((mid, track));
        }

        (new_assignments, newly_discovered_tracks)
    }

    fn get_active_track(&self, mid: &Mid) -> Option<&Arc<Track>> {
        let slot = self.slots.iter().find(|s| s.mid == *mid)?;
        let track_id = slot.track_id.as_ref()?;
        self.active_tracks.get(track_id)
    }
}
