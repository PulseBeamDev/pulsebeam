use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use pulsebeam_runtime::sync::spmc;
use str0m::media::{MediaKind, Rid};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::rtp::RtpPacket;

#[derive(Debug, Clone)]
pub struct KeyframeRequest {
    pub request: str0m::media::KeyframeRequest,
    pub requested_at: Instant,
}

#[derive(Debug, Eq, PartialEq, Hash)]
pub struct TrackMeta {
    pub id: Arc<crate::entity::TrackId>,
    pub kind: MediaKind,
    pub simulcast_rids: Option<Vec<Rid>>,
}

#[derive(Clone, Debug)]
pub struct SimulcastReceiver {
    pub meta: Arc<TrackMeta>,
    pub rid: Option<Rid>,
    pub channel: spmc::Receiver<RtpPacket>,
    pub keyframe_requester: watch::Sender<Option<KeyframeRequest>>,
    bitrate: Arc<AtomicU64>,
    paused: Arc<AtomicBool>,
}

impl SimulcastReceiver {
    pub fn request_keyframe(&self) {
        let request = str0m::media::KeyframeRequest {
            mid: self.meta.id.origin_mid,
            rid: self.rid,
            kind: str0m::media::KeyframeRequestKind::Pli,
        };
        let wrapped = KeyframeRequest {
            request,
            requested_at: Instant::now(),
        };
        if self.keyframe_requester.send(Some(wrapped)).is_err() {
            tracing::warn!(?request, "feedback channel is unavailable");
        }
    }

    pub fn estimated_bitrate(&self) -> u64 {
        self.bitrate.load(Ordering::Relaxed)
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
}

#[derive(Debug)]
pub struct SimulcastSender {
    pub rid: Option<Rid>,
    pub keyframe_requests: watch::Receiver<Option<KeyframeRequest>>,
    pub last_keyframe_requested_at: Option<Instant>,
    channel: spmc::Sender<RtpPacket>,
    bwe: BandwidthEstimator,
    paused: Arc<AtomicBool>,
}

impl SimulcastSender {
    pub fn get_keyframe_request(&mut self) -> Option<KeyframeRequest> {
        if !self.keyframe_requests.has_changed().unwrap_or(false) {
            return None;
        }

        let binding = self.keyframe_requests.borrow_and_update();
        let update = binding.as_ref()?;

        let now = Instant::now();
        if let Some(last_request_time) = self.last_keyframe_requested_at {
            if now.duration_since(last_request_time) < Duration::from_secs(1) {
                return None;
            }
        }
        self.last_keyframe_requested_at = Some(now);
        Some(update.clone())
    }

    pub fn push(&mut self, pkt: RtpPacket) {
        self.bwe.update(pkt.payload.len() + pkt.header.header_len);
        self.channel.send(pkt);
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }
}

pub struct TrackSender {
    pub meta: Arc<TrackMeta>,
    pub simulcast: Vec<SimulcastSender>,
}

impl TrackSender {
    pub fn push(&mut self, rid: Option<&Rid>, packet: RtpPacket) {
        let sender = self
            .simulcast
            .iter_mut()
            .find(|s| s.rid.as_ref() == rid)
            .expect("expected sender to always be available");
        sender.push(packet);
    }

    pub fn by_rid_mut(&mut self, rid: &Option<Rid>) -> Option<&mut SimulcastSender> {
        self.simulcast.iter_mut().find(|s| s.rid == *rid)
    }
}

#[derive(Clone, Debug)]
pub struct TrackReceiver {
    pub meta: Arc<TrackMeta>,
    pub simulcast: Vec<SimulcastReceiver>,
}

impl TrackReceiver {
    pub fn by_rid(&self, rid: Option<&Rid>) -> Option<&SimulcastReceiver> {
        self.simulcast.iter().find(|s| s.rid.as_ref() == rid)
    }
}

pub fn new(meta: Arc<TrackMeta>, capacity: usize) -> (TrackSender, TrackReceiver) {
    let mut simulcast_rids = if let Some(rids) = &meta.simulcast_rids {
        rids.iter().map(|rid| Some(*rid)).collect()
    } else {
        vec![None]
    };

    simulcast_rids.sort_by_key(|rid| rid.unwrap_or_default().to_string());

    let mut senders = Vec::new();
    let mut receivers = Vec::new();

    for rid in simulcast_rids {
        let (tx, rx) = spmc::channel(capacity);
        let (keyframe_tx, keyframe_rx) = watch::channel(None);

        let bitrate = match (meta.kind, rid) {
            (MediaKind::Audio, _) => 64_000,
            (MediaKind::Video, None) => 500_000,
            (MediaKind::Video, Some(r)) if r.starts_with('f') => 800_000,
            (MediaKind::Video, Some(r)) if r.starts_with('h') => 300_000,
            (MediaKind::Video, Some(r)) if r.starts_with('q') => 150_000,
            (MediaKind::Video, Some(rid)) => {
                tracing::warn!("use default bitrate due to unsupported rid: {rid}");
                500_000
            }
        };
        let bitrate = Arc::new(AtomicU64::new(bitrate));
        let bwe = BandwidthEstimator::new(bitrate.clone());
        let paused = Arc::new(AtomicBool::new(true));

        senders.push(SimulcastSender {
            rid,
            channel: tx,
            keyframe_requests: keyframe_rx,
            last_keyframe_requested_at: None,
            bwe,
            paused: paused.clone(),
        });
        receivers.push(SimulcastReceiver {
            meta: meta.clone(),
            rid,
            channel: rx,
            keyframe_requester: keyframe_tx,
            bitrate,
            paused,
        });
    }

    (
        TrackSender {
            meta: meta.clone(),
            simulcast: senders,
        },
        TrackReceiver {
            meta,
            simulcast: receivers,
        },
    )
}

#[derive(Debug)]
pub struct BandwidthEstimator {
    last_update: Instant,
    interval_bytes: usize,
    estimate: f64,
    shared: Arc<AtomicU64>,
    sample_count: u32,
}

impl BandwidthEstimator {
    pub fn new(shared: Arc<AtomicU64>) -> Self {
        let initial_bps = shared.load(Ordering::Relaxed);
        Self {
            last_update: Instant::now(),
            interval_bytes: 0,
            estimate: initial_bps as f64,
            shared,
            sample_count: 0,
        }
    }

    pub fn update(&mut self, pkt_bytes: usize) {
        self.interval_bytes += pkt_bytes + 50;
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_update);

        if elapsed >= Duration::from_millis(100) {
            let instant_bps = (self.interval_bytes as f64 * 8.0) / elapsed.as_secs_f64();
            self.interval_bytes = 0;
            self.sample_count += 1;
            let alpha = if self.sample_count < 5 { 0.50 } else { 0.25 };
            self.estimate = if self.estimate == 0.0 {
                instant_bps
            } else {
                (1.0 - alpha) * self.estimate + alpha * instant_bps
            };
            self.shared
                .store((self.estimate * 1.25) as u64, Ordering::Relaxed);
            self.last_update = now;
        }
    }
}
