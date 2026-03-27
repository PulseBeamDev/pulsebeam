use crate::api::{ApiError, CreateParticipantRequest, HttpApiClient};
use crate::manager::{Subscription, SubscriptionManager};
use crate::media::{KeyframeNotifier, KeyframeReceiver};
use crate::{MediaFrame, TransceiverDirection};
use futures_lite::StreamExt;
use http::Uri;
use pulsebeam_core::net::UdpSocket;
use pulsebeam_proto::prelude::*;
use pulsebeam_proto::signaling::Track;
use pulsebeam_proto::{namespace, signaling};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use str0m::IceConnectionState;
use str0m::bwe::{Bitrate, BweKind};
use str0m::channel::{ChannelConfig, ChannelData, ChannelId, Reliability};
use str0m::media::{KeyframeRequestKind, Rid, Simulcast, SimulcastLayer};
use str0m::{
    Candidate, Event, Input, Output, Rtc,
    media::{Direction, MediaAdded, MediaKind, Mid},
    net::{Protocol, Receive},
};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_stream::StreamMap;
use tokio_stream::wrappers::ReceiverStream;

const MIN_QUANTA: Duration = Duration::from_millis(1);
const STATE_DEBOUNCE: Duration = Duration::from_millis(300);
const BWE_SLOW_INTERVAL: Duration = Duration::from_millis(200);
// Sliding-window bitrate estimator: 20 buckets × 200 ms = 10 s window.
// A keyframe occupies at most 1 bucket out of 20, so it shifts the estimate by ≤5%.
const BITRATE_BUCKETS: usize = 20;
const BITRATE_WINDOW_SECS: f64 = BITRATE_BUCKETS as f64 * 0.5; // 10 s

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
// Smoothing factor used to low-pass filter the raw TWCC estimate before
// it is consumed by our debt algorithm.  A value of 1.0 bypasses the filter;
// smaller values make `available_bps` respond more slowly to sudden drops.
// In normal internet operation the estimator already behaves sanely, so this
// only matters on zero‑delay links where tiny bursts can trigger over‑use.
const BW_SMOOTHING_ALPHA: f64 = 0.2;
const KEYFRAME_REQUEST_THROTTLE: Duration = Duration::from_secs(1);

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
    /// Bitrate estimate derived from a sliding ring-buffer window.
    /// Each bucket holds the raw bytes received in one BWE_SLOW_INTERVAL period.
    /// bps = sum(byte_buckets) * 8 / BITRATE_WINDOW_SECS.
    /// Stable across keyframe bursts: a single IDR occupies ≤1 bucket out of 20.
    bps: f64,
    paused: bool,
    /// Bytes accumulated from incoming frames in the current bucket interval.
    frame_bytes_acc: u64,
    /// Ring buffer of per-bucket byte counts (one bucket per BWE_SLOW_INTERVAL).
    byte_buckets: [u64; BITRATE_BUCKETS],
    /// Index of the next bucket to write.
    bucket_pos: usize,
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

    fn has_layers(&self) -> bool {
        !self.order.is_empty()
    }

    fn register(&mut self, mid: Mid, rid: Option<Rid>, notifier: KeyframeNotifier) {
        let key = (mid, rid);
        // q (rank 0) and non-simulcast tracks start active immediately.
        // h and f start paused; debt-free ticks resume them once BWE confirms budget.
        let paused = rid.is_some() && rid_quality_rank(rid) > 0;
        self.order.push(key);
        // Sort so index 0 = highest priority (q), last = lowest priority (f).
        self.order.sort_by_key(|(_, rid)| rid_quality_rank(*rid));
        // Pre-fill the ring buffer with the seed bitrate so the upgrade gate sees a
        // realistic cost before the first full 10 s window of real measurements fills in.
        let seed_bytes = (layer_seed_bps(rid) * 0.5 / 8.0) as u64;
        self.states.insert(
            key,
            LayerState {
                bps: layer_seed_bps(rid),
                paused,
                frame_bytes_acc: 0,
                byte_buckets: [seed_bytes; BITRATE_BUCKETS],
                bucket_pos: 0,
            },
        );
        self.notifiers.insert(key, notifier);
    }

    fn update_available(&mut self, bw: Bitrate) {
        self.available_bps = bw.as_f64();
    }

    /// Called for every incoming frame regardless of whether the layer is currently paused.
    /// This gives us a true picture of what the encoder is producing.
    fn record_frame(&mut self, mid: Mid, rid: Option<Rid>, byte_len: usize, _now: Instant) {
        let key = (mid, rid);
        let Some(s) = self.states.get_mut(&key) else {
            return;
        };
        s.frame_bytes_acc += byte_len as u64;
        // tick() drains the accumulator; this just fills it.
    }

    /// Rotate the bitrate ring buffer and recompute bps.
    /// Called once per BWE_SLOW_INTERVAL tick, before allocation decisions.
    ///
    /// Each call stores this interval's frame bytes into the oldest bucket slot
    /// and computes bps = total_bytes_in_window * 8 / BITRATE_WINDOW_SECS.
    /// Because the window spans 20 intervals (10 s), a single IDR frame burst
    /// occupies at most one bucket and shifts the estimate by ≤5% — keyframes
    /// are effectively invisible to the allocation logic.
    fn flush_frame_bitrates(&mut self, _now: Instant) {
        for s in self.states.values_mut() {
            // Overwrite the oldest bucket with this interval's byte count.
            s.byte_buckets[s.bucket_pos] = s.frame_bytes_acc;
            s.bucket_pos = (s.bucket_pos + 1) % BITRATE_BUCKETS;
            s.frame_bytes_acc = 0;

            let total_bytes: u64 = s.byte_buckets.iter().sum();
            s.bps = (total_bytes as f64 * 8.0) / BITRATE_WINDOW_SECS;
        }
    }

    fn is_paused(&self, mid: Mid, rid: Option<Rid>) -> bool {
        self.states.get(&(mid, rid)).map_or(false, |s| s.paused)
    }

    fn request_keyframe(&mut self, mid: Mid, rid: Option<Rid>, kind: KeyframeRequestKind) {
        // Throttle PLI requests to avoid encoder storm like libwebrtc.
        if kind == KeyframeRequestKind::Pli {
            let key = (mid, rid);
            let now = Instant::now();
            if let Some(last) = self.last_keyframe_request.get(&key) {
                if now.duration_since(*last) < KEYFRAME_REQUEST_THROTTLE {
                    tracing::debug!(mid = ?mid, rid = ?rid, "throttling repeated PLI");
                    return;
                }
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
        self.flush_frame_bitrates(now);

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
                .find(|k| self.states.get(*k).map_or(false, |s| !s.paused) && k.1.is_some())
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
                .find(|k| self.states.get(*k).map_or(false, |s| s.paused))
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

fn new_remote_track(mid: Mid, track: Arc<Track>) -> (RemoteTrackTx, RemoteTrackRx) {
    let (ch_tx, ch_rx) = mpsc::channel(8);
    let tx = RemoteTrackTx {
        track: track.clone(),
        tx: ch_tx,
    };
    let rx = RemoteTrackRx {
        mid,
        track,
        rx: ch_rx,
    };

    (tx, rx)
}

#[derive(Debug, Clone)]
pub struct RemoteTrackTx {
    pub track: Arc<Track>,
    tx: mpsc::Sender<MediaFrame>,
}

impl RemoteTrackTx {
    pub fn try_send(&self, frame: MediaFrame) {
        let _ = self.tx.try_send(frame);
    }

    pub async fn send(&self, frame: MediaFrame) {
        let _ = self.tx.send(frame).await;
    }
}

#[derive(Debug)]
pub struct RemoteTrackRx {
    pub mid: Mid,
    pub track: Arc<Track>,
    rx: mpsc::Receiver<MediaFrame>,
}

impl RemoteTrackRx {
    pub async fn recv(&mut self) -> Option<MediaFrame> {
        self.rx.recv().await
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
    #[error("Comand Error: {0}")]
    Command(#[from] mpsc::error::SendError<AgentCommand>),
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
}

impl AgentBuilder {
    pub fn new(api: HttpApiClient, udp_socket: UdpSocket) -> AgentBuilder {
        Self {
            api,
            udp_socket,
            tracks: Vec::new(),
            local_ips: Vec::new(),
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

    pub async fn connect(mut self, room_id: &str) -> Result<Agent, AgentError> {
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
            .enable_bwe(Some(Bitrate::kbps(300)))
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
        for ip in self.local_ips {
            let addr = SocketAddr::new(ip, port);
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

        // matching SFU's reserved channel
        rtc.direct_api().create_data_channel(ChannelConfig {
            label: "".to_string(),
            ordered: true,
            reliability: Reliability::Reliable,
            negotiated: Some(1),
            protocol: "v1".to_string(),
        });

        let mut sdp = rtc.sdp_api();
        // Add a dummy channel to ensure the SDP offer contains an m=application section.
        // This is necessary for SCTP to be enabled.
        sdp.add_channel("sctp-enable".to_string());
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
        let (offer, pending) = sdp.apply().expect("offer is required");
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

        let (cmd_tx, cmd_rx) = mpsc::channel(100);
        let (event_tx, event_rx) = mpsc::channel(100);

        let actor = AgentActor {
            api: self.api,
            addr,
            rtc,
            stats: AgentStats::default(),
            socket: self.udp_socket,
            buf: vec![0u8; 2048],
            cmd_rx,
            event_tx,
            resource_uri: resp.resource_uri,
            participant_id: resp.participant_id.clone(),
            senders: StreamMap::new(),
            pending: PendingState::new(),
            slot_manager: SlotManager::new(),
            disconnected_reason: None,
            signaling_cid,
            layer_ctrl: LayerController::new(),
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
        };

        tokio::spawn(async move {
            actor.run(medias).await;
        });

        Ok(Agent {
            room_id: room_id.to_string(),
            participant_id: resp.participant_id,
            cmd_tx,
            event_rx,
        })
    }
}

#[derive(Debug)]
pub enum AgentCommand {
    Disconnect,
    GetStats(oneshot::Sender<AgentStats>),

    /// Assigns a specific Track to a specific Receiver Slot (MID) at a target resolution.
    ///
    /// - `track_id`: The remote track to subscribe to.
    /// - `height`: Desired resolution (e.g. 720, 1080).
    ///   Use `0` to pause the stream while keeping the assignment.
    Subscribe {
        track_id: String,
        height: u32,
    },

    /// Stops receiving media on the specified slot and clears the assignment.
    ///
    /// The Actor will look up the `track_id` currently assigned to this `mid`
    /// and send a `VideoRequest` with `height: 0`.
    Unsubscribe {
        track_id: String,
    },

    /// Declarative subscription state update.
    SetSubscriptions(Vec<Subscription>),

    /// Set video encoding preset for outgoing media.
    SetPreset(VideoPreset),
}

#[derive(Debug)]
pub enum AgentEvent {
    LocalTrackAdded(LocalTrack),
    /// The server announced a remote track (metadata only) even if the client
    /// has not yet subscribed to it.
    RemoteTrackDiscovered(pulsebeam_proto::signaling::Track),
    RemoteTrackAdded(RemoteTrackRx),
    Connected,
    Disconnected(String),
}

pub struct Agent {
    room_id: String,
    participant_id: String,
    cmd_tx: mpsc::Sender<AgentCommand>,
    event_rx: mpsc::Receiver<AgentEvent>,
}

impl Agent {
    pub fn participant_id(&self) -> &str {
        &self.participant_id
    }

    pub async fn next_event(&mut self) -> Option<AgentEvent> {
        self.event_rx.recv().await
    }

    pub async fn get_stats(&self) -> Option<AgentStats> {
        let (stats_tx, stats_rx) = oneshot::channel();
        let _ = self.cmd_tx.send(AgentCommand::GetStats(stats_tx)).await;
        stats_rx.await.ok()
    }

    pub async fn send(&self, cmd: AgentCommand) -> Result<(), AgentError> {
        self.cmd_tx.send(cmd).await?;
        Ok(())
    }

    pub async fn disconnect(&mut self) -> Result<(), AgentError> {
        let _ = self.cmd_tx.send(AgentCommand::Disconnect).await;

        Ok(())
    }

    pub async fn set_subscriptions(&self, subs: Vec<Subscription>) -> Result<(), AgentError> {
        self.cmd_tx
            .send(AgentCommand::SetSubscriptions(subs))
            .await?;
        Ok(())
    }

    pub async fn set_preset(&self, preset: VideoPreset) -> Result<(), AgentError> {
        self.cmd_tx.send(AgentCommand::SetPreset(preset)).await?;
        Ok(())
    }
}

struct AgentActor {
    addr: SocketAddr,
    rtc: Rtc,
    socket: UdpSocket,
    buf: Vec<u8>,
    stats: AgentStats,
    cmd_rx: mpsc::Receiver<AgentCommand>,
    event_tx: mpsc::Sender<AgentEvent>,

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

    sub_manager: SubscriptionManager,
    preset: VideoPreset,

    retry_count: u32,
    is_reconnecting: bool,
    reconnect_deadline: Option<Instant>,
}

impl AgentActor {
    async fn run(mut self, medias: Vec<MediaAdded>) {
        for media in medias {
            self.handle_media_added(media);
        }

        let debounce_timer = tokio::time::sleep(Duration::ZERO);
        tokio::pin!(debounce_timer);
        let sleep = tokio::time::sleep(MIN_QUANTA);
        tokio::pin!(sleep);
        // Periodic BWE reallocation tick (handles upgrade/recovery paths).
        let mut bwe_slow_timer = tokio::time::interval(BWE_SLOW_INTERVAL);
        let reconnect_timer = tokio::time::sleep(Duration::ZERO);
        tokio::pin!(reconnect_timer);

        while let Some(deadline) = self.poll_rtc().await {
            let now = Instant::now();
            let adjusted_deadline = if deadline <= now {
                now + MIN_QUANTA
            } else {
                deadline
            };

            if sleep.deadline() != adjusted_deadline {
                sleep.as_mut().reset(adjusted_deadline);
            }

            if let Some(reconnect_at) = self.reconnect_deadline {
                if reconnect_timer.deadline() != reconnect_at {
                    reconnect_timer.as_mut().reset(reconnect_at);
                }
            }

            if let Some(deadline) = self.pending.deadline {
                if debounce_timer.deadline() != deadline {
                    debounce_timer.as_mut().reset(deadline);
                }
            }

            tokio::select! {
                biased;
                Some(cmd) = self.cmd_rx.recv() => {
                    self.handle_command(cmd, now);
                }
                // If we have a pending state update (for subscription changes),
                // flush it when the debounce timer fires.
                _ = &mut debounce_timer, if self.pending.deadline.is_some() => {
                    self.flush_pending_state(now);
                }
                res = self.socket.recv_from(&mut self.buf) => {
                    if let Ok((n, source)) = res {
                         let _ = self.rtc.handle_input(Input::Receive(
                            Instant::now().into(),
                            Receive {
                                proto: Protocol::Udp,
                                source,
                                destination: self.addr,
                                contents: self.buf[..n].try_into().unwrap(),
                            }
                        ));
                    }
                }

                // Drain paused layers so the looper task never blocks.
                Some(((mid, rid), frame)) = self.senders.next() => {
                    let paused = self.layer_ctrl.is_paused(mid, rid);
                    // Measure the encoder output bitrate BEFORE the pause gate.
                    // This ensures we always know what the encoder *would* cost
                    // even when the layer is suppressed.
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
                            let _ = writer.write(pt, frame.capture_time.into(), frame.ts, frame.data);
                        } else {
                            tracing::warn!(?mid, "no writer found for mid");
                        }
                    }
                }

                _ = bwe_slow_timer.tick(), if self.layer_ctrl.has_layers() => {
                    let desired_bps = self.layer_ctrl.tick(Instant::now());
                    let desired = Bitrate::bps((desired_bps * 1.5) as u64);
                    tracing::debug!("desired bitrate: {}", desired);
                    self.rtc.bwe().set_desired_bitrate(desired);
                }

                 _ = &mut sleep => {
                    if self.rtc.handle_input(Input::Timeout(Instant::now().into())).is_err() {
                         self.emit(AgentEvent::Disconnected("RTC Timeout".into()));
                    }
                }

                _ = &mut reconnect_timer, if self.reconnect_deadline.is_some() => {
                    self.perform_reconnect().await;
                }
            }
        }

        let _ = self
            .api
            .delete_participant_by_uri(self.resource_uri.clone())
            .await;
    }

    async fn poll_rtc(&mut self) -> Option<Instant> {
        loop {
            match self.rtc.poll_output() {
                Ok(Output::Transmit(tx)) => {
                    let _ = self.socket.send_to(&tx.contents, tx.destination).await;
                }
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
                            self.pending.deadline.replace(Instant::now());
                        }
                    }
                    Event::ChannelData(data) => {
                        self.handle_signaling_data(data);
                    }
                    Event::MediaAdded(media) => self.handle_media_added(media),

                    Event::MediaData(data) => {
                        if let Some(tx) = self.slot_manager.get_sender(&data.mid) {
                            tx.try_send(data.into());
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

    fn handle_command(&mut self, cmd: AgentCommand, now: Instant) {
        match cmd {
            AgentCommand::Disconnect => self.rtc.disconnect(),
            AgentCommand::GetStats(stats_tx) => {
                let _ = stats_tx.send(self.stats.clone());
            }
            AgentCommand::Subscribe { .. } | AgentCommand::Unsubscribe { .. } => {
                tracing::warn!("Manual subscribe/unsubscribe is deprecated, use SetSubscriptions");
            }
            AgentCommand::SetSubscriptions(subs) => {
                tracing::debug!("processing SetSubscriptions: {:?}", subs);
                self.sub_manager.set_desired(subs);
                self.pending.deadline.replace(now + STATE_DEBOUNCE);
                // Make sure we attempt to send the new desired state immediately.
                self.flush_pending_state(now);
            }
            AgentCommand::SetPreset(preset) => {
                self.preset = preset;
                // In a more complete implementation, we would update RTCPeerConnection parameters here
                // if the underlying WebRTC implementation supports it.
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
                let (new_remote_tracks, new_discovered_tracks) = self.slot_manager.sync(update);
                for track in new_discovered_tracks {
                    self.emit(AgentEvent::RemoteTrackDiscovered(track));
                }
                for rx in new_remote_tracks {
                    tracing::info!("Emitting RemoteTrackAdded for MID: {:?}", rx.mid);
                    self.emit(AgentEvent::RemoteTrackAdded(rx));
                }
            }
            signaling::server_message::Payload::Error(err) => {
                tracing::warn!("signaling error: {}", err);
            }
        }
    }

    fn emit(&self, event: AgentEvent) {
        let _ = self.event_tx.try_send(event);
    }

    fn flush_pending_state(&mut self, now: Instant) {
        let channel_exists = self.rtc.channel(self.signaling_cid).is_some();
        tracing::debug!(
            "flush_pending_state: signaling_cid={:?}, exists={}",
            self.signaling_cid,
            channel_exists
        );

        let Some(mut ch) = self.rtc.channel(self.signaling_cid) else {
            tracing::debug!("signaling channel not ready yet, will retry later");
            // Keep retrying in the future
            self.pending.deadline.replace(now + STATE_DEBOUNCE);
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
                signaling::ClientIntent { requests },
            )),
        };
        let encoded = msg.encode_to_vec();
        if let Err(err) = ch.write(true, encoded.as_slice()) {
            tracing::warn!("failed to send signaling: {:?}", err);
            // Retry later
            self.pending.deadline.replace(now + STATE_DEBOUNCE);
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
}

struct ReceiverSlot {
    mid: Mid,
    track_id: Option<TrackId>,
}

struct SlotManager {
    // Tracks discovered in the room but not yet assigned to a slot.
    pending_tracks: HashMap<TrackId, Arc<Track>>,
    // Active track transmitters.
    remote_tracks: HashMap<TrackId, RemoteTrackTx>,
    // The fixed set of Receive Transceivers (MIDs) available to this Agent.
    slots: Vec<ReceiverSlot>,
}

impl SlotManager {
    fn new() -> Self {
        Self {
            pending_tracks: HashMap::new(),
            slots: Vec::new(),
            remote_tracks: HashMap::new(),
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
    ) -> (Vec<RemoteTrackRx>, Vec<pulsebeam_proto::signaling::Track>) {
        tracing::info!("Syncing SlotManager state with update: {:?}", update);
        let mut new_remote_tracks = Vec::new();
        let mut newly_discovered_tracks = Vec::new();

        // 1. Handle track removals
        for t in update.tracks_remove {
            self.pending_tracks.remove(&t);
            self.remote_tracks.remove(&t);
        }

        // 2. Handle new tracks (store as pending if not already active or pending)
        for t in update.tracks_upsert {
            if self.remote_tracks.contains_key(&t.id) {
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
                let (tx, rx) = new_remote_track(s.mid, track);
                self.remote_tracks.insert(a.track_id, tx);
                new_remote_tracks.push(rx);
            } else {
                tracing::debug!(
                    "Track {} already active or not found in pending",
                    a.track_id
                );
            }
        }

        // 5. Handle cases where the assignment arrived before the track metadata.
        // If we already know about the track (in pending_tracks) and it is assigned,
        // ensure we create a RemoteTrack for it.
        for slot in &self.slots {
            let Some(track_id) = &slot.track_id else {
                continue;
            };

            if self.remote_tracks.contains_key(track_id) {
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
            let (tx, rx) = new_remote_track(slot.mid, track);
            self.remote_tracks.insert(track_id.clone(), tx);
            new_remote_tracks.push(rx);
        }

        (new_remote_tracks, newly_discovered_tracks)
    }

    fn get_sender(&self, mid: &Mid) -> Option<&RemoteTrackTx> {
        let slot = self.slots.iter().find(|s| s.mid == *mid)?;
        let track_id = slot.track_id.as_ref()?;
        self.remote_tracks.get(track_id)
    }
}
