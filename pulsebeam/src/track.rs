use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
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
    pub bitrate: Arc<AtomicU64>,
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

        tracing::debug!(?request, "keyframe request is sent");
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

        // A new request has arrived. Check if we should process it.
        let binding = self.keyframe_requests.borrow_and_update();
        let Some(update) = binding.as_ref() else {
            return None; // The update was to clear the request.
        };

        // Check the throttle timer.
        let now = tokio::time::Instant::now();
        if let Some(last_request_time) = self.last_keyframe_requested_at
            && now.duration_since(last_request_time) < Duration::from_secs(1)
        {
            // It's too soon. Ignore this request and DO NOT update the timer.
            return None;
        }

        // If we reach here, it's either the first request ever, or the 1-second
        // cooldown has passed. We can process it.

        // Update the timer ONLY when we are actually processing a request.
        self.last_keyframe_requested_at = Some(now);

        // Return the keyframe request.
        Some(update.clone())
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
}

/// Construct a new Track (returns sender + receiver).
pub fn new(meta: Arc<TrackMeta>, capacity: usize) -> (TrackSender, TrackReceiver) {
    let mut simulcast_rids = if let Some(rids) = &meta.simulcast_rids {
        rids.iter().map(|rid| Some(*rid)).collect()
    } else {
        vec![None]
    };

    // "f" -> "h" -> "q"
    simulcast_rids.sort_by_key(|rid| rid.unwrap_or_default().to_string());

    let mut senders = Vec::new();
    let mut receivers = Vec::new();

    for rid in simulcast_rids {
        let (tx, rx) = spmc::channel(capacity);
        let (keyframe_tx, keyframe_rx) = watch::channel(None);

        let bitrate = match (meta.kind, rid) {
            (MediaKind::Audio, _) => 64_000, // Reasonable audio starting point
            (MediaKind::Video, None) => 500_000, // Start with medium quality
            (MediaKind::Video, Some(rid)) if rid.starts_with("f") => 800_000, // Start lower than max
            (MediaKind::Video, Some(rid)) if rid.starts_with("h") => 300_000, // Start lower
            (MediaKind::Video, Some(rid)) if rid.starts_with("q") => 150_000, // Start higher than min
            (MediaKind::Video, Some(rid)) => {
                tracing::warn!("use default bitrate due to unsupported rid: {rid}");
                500_000
            }
        };
        let bitrate = Arc::new(AtomicU64::new(bitrate));

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
    shared: Arc<AtomicU64>,
    /// Number of samples collected (for warm-up period)
    sample_count: u32,
}

impl BandwidthEstimator {
    pub fn new(shared: Arc<AtomicU64>) -> Self {
        // Start with the provided initial estimate
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
        self.interval_bytes += pkt_bytes + 50; // rough IPv6 header estimate

        let now = Instant::now();
        let elapsed = now.duration_since(self.last_update);

        // Update every 100ms for faster reaction
        if elapsed >= Duration::from_millis(100) {
            // Instantaneous bps
            let instant_bps = (self.interval_bytes as f64 * 8.0) / elapsed.as_secs_f64();
            self.interval_bytes = 0;
            self.sample_count += 1;

            // EWMA smoothing with faster reaction
            // - During warm-up (first 5 samples): react very fast (alpha=0.5)
            // - After warm-up: moderate smoothing (alpha=0.25)
            let alpha = if self.sample_count < 5 {
                0.50 // React fast during warm-up
            } else {
                0.25 // Moderate smoothing after warm-up
            };

            if self.estimate == 0.0 {
                // First sample - initialize with current value
                self.estimate = instant_bps;
            } else {
                // EWMA: new_estimate = (1-alpha) * old + alpha * new
                self.estimate = (1.0 - alpha) * self.estimate + alpha * instant_bps;
            }

            // Store the estimate directly without additional dampening
            self.shared.store(self.estimate as u64, Ordering::Relaxed);

            self.last_update = now;
        }
    }

    /// Reset the estimator (e.g., when layer switches)
    pub fn reset(&mut self) {
        self.interval_bytes = 0;
        self.sample_count = 0;
        self.last_update = Instant::now();
        // Keep the current estimate as a baseline
    }
}
