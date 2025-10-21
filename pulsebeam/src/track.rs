use std::{
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use str0m::{
    media::{MediaKind, Rid},
    rtp::RtpPacket,
};
use tokio::sync::watch;
use tokio::time::Instant;

use pulsebeam_runtime::sync::spmc;

#[derive(Debug, Clone)]
pub struct KeyframeRequest {
    pub request: str0m::media::KeyframeRequest,
    pub requested_at: tokio::time::Instant,
}

/// Metadata for a track — wraps `TrackMeta` in the app layer.
#[derive(Debug, Eq, PartialEq, Hash)]
pub struct TrackMeta {
    pub id: Arc<crate::entity::TrackId>,
    pub kind: str0m::media::MediaKind,
    pub simulcast_rids: Option<Vec<Rid>>,
}

/// Simulcast receiver for one RID.
#[derive(Clone, Debug)]
pub struct SimulcastReceiver {
    pub meta: Arc<TrackMeta>,
    pub rid: Option<Rid>,
    pub channel: spmc::Receiver<RtpPacket>,
    /// Used to request a keyframe from the sender.
    pub keyframe_requester: watch::Sender<Option<KeyframeRequest>>,

    /// Receiver only reads. The publisher will estimate this.
    pub bitrate: Arc<AtomicU32>,
}

impl SimulcastReceiver {
    /// Request a keyframe on all simulcast layers.
    pub fn request_keyframe(&self) {
        let request = str0m::media::KeyframeRequest {
            mid: self.meta.id.origin_mid,
            rid: self.rid,
            kind: str0m::media::KeyframeRequestKind::Pli,
        };
        let wrapped = KeyframeRequest {
            request,
            requested_at: tokio::time::Instant::now(),
        };
        let Ok(_) = self.keyframe_requester.send(Some(wrapped)) else {
            tracing::warn!(?request, "feedback channel is unavailable");
            return;
        };
    }
}

/// Simulcast sender for one RID.
#[derive(Debug)]
pub struct SimulcastSender {
    pub rid: Option<Rid>,
    /// Used to receive keyframe requests from the receiver.
    pub keyframe_requests: watch::Receiver<Option<KeyframeRequest>>,

    pub last_keyframe_requested_at: Option<tokio::time::Instant>,

    channel: spmc::Sender<RtpPacket>,
    bwe: BandwidthEstimator,
}

impl SimulcastSender {
    pub fn get_keyframe_request(&mut self) -> Option<KeyframeRequest> {
        let has_changed = self
            .keyframe_requests
            .has_changed()
            .expect("keyframe request channel must be opened when publisher is alive.");

        if !has_changed {
            return None;
        }

        let binding = self.keyframe_requests.borrow_and_update();
        let update = binding.as_ref()?;

        let res = if let Some(last) = self.last_keyframe_requested_at {
            if last.elapsed() < Duration::from_secs(1) {
                Some(update.clone())
            } else {
                None
            }
        } else {
            None
        };
        self.last_keyframe_requested_at = Some(tokio::time::Instant::now());
        res
    }

    pub fn send(&mut self, pkt: RtpPacket) {
        self.bwe.update(pkt.payload.len() + pkt.header.header_len);
        self.channel.send(pkt);
    }
}

/// Sender half of a track (typically owned by publisher/participant)
pub struct TrackSender {
    pub meta: Arc<TrackMeta>,
    pub simulcast: Vec<SimulcastSender>,
}

impl TrackSender {
    pub fn send(&mut self, rid: Option<&Rid>, pkt: RtpPacket) {
        let sender = self
            .simulcast
            .iter_mut()
            .find(|s| s.rid.as_ref() == rid)
            .expect("expected sender to always available");

        tracing::trace!(?self.meta, ?pkt, ?rid, ?sender, "sent to track");
        sender.send(pkt);
    }
}

/// Receiver half of a track (typically consumed by subscribers)
#[derive(Clone, Debug)]
pub struct TrackReceiver {
    pub meta: Arc<TrackMeta>,
    pub simulcast: Vec<SimulcastReceiver>,
}

impl TrackReceiver {
    /// Convenience for accessing a specific RID
    pub fn by_rid(&self, rid: Option<&Rid>) -> Option<SimulcastReceiver> {
        self.simulcast
            .iter()
            .find(|s| s.rid.as_ref() == rid)
            .cloned()
    }

    pub fn by_default(&mut self) -> Option<&mut SimulcastReceiver> {
        for simulcast in &mut self.simulcast {
            let Some(rid) = simulcast.rid else {
                return Some(simulcast);
            };

            if rid.starts_with('f') {
                return Some(simulcast);
            }
        }
        None
    }
}

/// Construct a new Track (returns sender + receiver).
pub fn new(meta: Arc<TrackMeta>, capacity: usize) -> (TrackSender, TrackReceiver) {
    let simulcast_rids = if let Some(rids) = &meta.simulcast_rids {
        rids.iter().map(|rid| Some(*rid)).collect()
    } else {
        vec![None]
    };

    let mut senders = Vec::new();
    let mut receivers = Vec::new();

    for rid in simulcast_rids {
        let (tx, rx) = spmc::channel(capacity);
        let (keyframe_tx, keyframe_rx) = watch::channel(None);

        let bitrate = match (meta.kind, rid) {
            (MediaKind::Audio, _) => 64_000,
            (MediaKind::Video, None) => 1_200_000,
            (MediaKind::Video, Some(rid)) if rid.starts_with("f") => 1_200_000,
            (MediaKind::Video, Some(rid)) if rid.starts_with("h") => 500_000,
            (MediaKind::Video, Some(rid)) if rid.starts_with("q") => 200_000,
            (MediaKind::Video, Some(rid)) => {
                tracing::warn!("use default bitrate due to unsupported rid: {rid}");
                1_200_000
            }
        };
        let bitrate = Arc::new(AtomicU32::new(bitrate));

        let bwe = BandwidthEstimator::new(bitrate.clone());
        senders.push(SimulcastSender {
            rid,
            channel: tx,
            keyframe_requests: keyframe_rx,
            last_keyframe_requested_at: None,
            bwe,
        });

        receivers.push(SimulcastReceiver {
            meta: meta.clone(),
            rid,
            channel: rx,
            keyframe_requester: keyframe_tx,
            bitrate,
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

/// Conservative long-term bandwidth estimator
#[derive(Debug)]
pub struct BandwidthEstimator {
    last_update: Instant,
    interval_bytes: usize,
    estimate: f64, // EWMA in bps
    shared: Arc<AtomicU32>,
}

impl BandwidthEstimator {
    pub fn new(shared: Arc<AtomicU32>) -> Self {
        let initial_bps = shared.load(Ordering::Relaxed);
        Self {
            last_update: Instant::now(),
            interval_bytes: 0,
            estimate: initial_bps as f64,
            shared,
        }
    }

    pub fn update(&mut self, pkt_bytes: usize) {
        self.interval_bytes += pkt_bytes + 50; // rough IPv6 header estimate

        let now = Instant::now();
        let elapsed = now.duration_since(self.last_update);
        if elapsed >= Duration::from_millis(100) {
            // instantaneous bps
            let instant_bps = (self.interval_bytes as f64 * 8.0) / elapsed.as_secs_f64();
            self.interval_bytes = 0;

            // EWMA smoothing for long-term average
            // alpha smaller => faster reaction to bursts
            self.estimate = 0.90 * self.estimate + 0.10 * instant_bps;

            // add 15% headroom for bursts
            let safe_bps = (self.estimate * 1.15) as u32;
            self.shared.store(safe_bps, Ordering::Relaxed);

            self.last_update = now;
        }
    }
}
