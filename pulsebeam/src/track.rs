use std::{sync::Arc, time::Duration};

use pulsebeam_runtime::sync::spmc;
use str0m::media::{Frequency, KeyframeRequest, KeyframeRequestKind, MediaKind, Mid, Rid};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::rtp::{
    self, RtpPacket,
    monitor::{StreamMonitor, StreamState},
    sync::Synchronizer,
};

#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum SimulcastQuality {
    Undefined = 0,
    Low = 1,
    Medium = 2,
    High = 3,
}

impl std::fmt::Debug for SimulcastQuality {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let txt = match self {
            Self::Undefined => "undefined",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        };
        f.write_str(txt)
    }
}

#[derive(Debug, Eq, PartialEq, Hash)]
pub struct TrackMeta {
    pub id: Arc<crate::entity::TrackId>,
    pub origin_participant: Arc<crate::entity::ParticipantId>,
    pub kind: MediaKind,
    pub simulcast_rids: Option<Vec<Rid>>,
}

#[derive(Clone, Debug)]
pub struct SimulcastReceiver {
    pub meta: Arc<TrackMeta>,
    pub rid: Option<Rid>,
    pub quality: SimulcastQuality,
    pub channel: spmc::Receiver<RtpPacket>,
    pub keyframe_requester: watch::Sender<Option<KeyframeRequestKind>>,
    pub state: StreamState,
}

impl SimulcastReceiver {
    pub fn request_keyframe(&self, kind: KeyframeRequestKind) {
        if self.keyframe_requester.send(Some(kind)).is_err() {
            tracing::warn!(?kind, "feedback channel is unavailable");
        }
    }
}

/// Represents the state of a keyframe request.
#[derive(Debug, Default)]
enum KeyframeRequestState {
    /// No keyframe has been requested.
    #[default]
    Idle,
    /// A keyframe has been requested, and we are waiting for the debounce duration to pass.
    Debouncing { requested_at: Instant },
}

#[derive(Debug)]
pub struct SimulcastSender {
    pub mid: Mid,
    pub rid: Option<Rid>,
    pub quality: SimulcastQuality,
    pub monitor: StreamMonitor,
    synchronizer: Synchronizer,
    channel: spmc::Sender<RtpPacket>,

    keyframe_request_state: KeyframeRequestState,
    keyframe_debounce_duration: Duration,
    keyframe_requests: watch::Receiver<Option<KeyframeRequestKind>>,
}

impl SimulcastSender {
    /// Checks for and returns a keyframe request if the debounce conditions are met.
    pub fn get_keyframe_request(&mut self) -> Option<KeyframeRequest> {
        // Check if a new request signal has arrived.
        if self.keyframe_requests.has_changed().unwrap_or(false) {
            // Borrow and update to mark this change as seen.
            let binding = self.keyframe_requests.borrow_and_update();
            if binding.is_some() {
                // A new request came in. Start or reset the debounce timer.
                self.keyframe_request_state = KeyframeRequestState::Debouncing {
                    requested_at: Instant::now(),
                };
            }
        }

        // Check if we are in the debouncing state and if the timer has elapsed.
        if let KeyframeRequestState::Debouncing { requested_at } = self.keyframe_request_state
            && Instant::now().duration_since(requested_at) >= self.keyframe_debounce_duration
        {
            // Debounce duration has passed. Transition to Idle and issue the request.
            self.keyframe_request_state = KeyframeRequestState::Idle;

            // Return the latest request from the channel.
            let kind = self.keyframe_requests.borrow();
            return kind.map(|k| KeyframeRequest {
                kind: k,
                rid: self.rid,
                mid: self.mid,
            });
        }

        None
    }

    pub fn push(&mut self, pkt: RtpPacket) {
        // self.jb.push(pkt)
        self.forward(pkt);
    }

    pub fn poll_stats(&mut self, now: Instant, is_any_sibling_active: bool) {
        self.monitor.poll(now, is_any_sibling_active);
    }

    pub fn poll(&mut self, now: Instant) -> Option<Instant> {
        None
    }

    fn forward(&mut self, pkt: RtpPacket) {
        if let KeyframeRequestState::Debouncing { .. } = self.keyframe_request_state
            && pkt.is_keyframe_start
        {
            tracing::debug!("Keyframe received, cancelling pending request.");
            self.keyframe_request_state = KeyframeRequestState::Idle;
        }

        // RTP Pipeline
        let pkt = self.synchronizer.process(pkt);
        self.monitor
            .process_packet(&pkt, pkt.payload.len() + pkt.raw_header.header_len);
        self.channel.send(pkt);
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

    pub fn poll(&mut self, now: Instant) -> Option<Instant> {
        self.simulcast
            .iter_mut()
            .filter_map(|layer| layer.poll(now))
            .min()
    }

    pub fn poll_stats(&mut self, now: Instant) {
        let total_active_streams = self
            .simulcast
            .iter()
            .filter(|s| !s.monitor.shared_state().is_inactive())
            .count();

        for layer in self.simulcast.iter_mut() {
            let is_current_layer_active = !layer.monitor.shared_state().is_inactive();
            let is_any_sibling_active = if is_current_layer_active {
                total_active_streams > 1
            } else {
                total_active_streams > 0
            };

            layer.poll_stats(now, is_any_sibling_active);
        }
    }
}

#[derive(Clone, Debug)]
pub struct TrackReceiver {
    pub meta: Arc<TrackMeta>,
    pub simulcast: Vec<SimulcastReceiver>,
}

impl TrackReceiver {
    pub fn by_rid(&self, rid: &Option<Rid>) -> Option<&SimulcastReceiver> {
        self.simulcast.iter().find(|s| s.rid == *rid)
    }

    pub fn higher_quality(&self, current: SimulcastQuality) -> Option<&SimulcastReceiver> {
        let next_quality = match current {
            SimulcastQuality::Undefined => Some(SimulcastQuality::Low),
            SimulcastQuality::Low => Some(SimulcastQuality::Medium),
            SimulcastQuality::Medium => Some(SimulcastQuality::High),
            SimulcastQuality::High => None,
        };
        if let Some(next) = next_quality {
            self.simulcast.iter().find(|s| s.quality == next)
        } else {
            None
        }
    }

    pub fn lower_quality(&self, current: SimulcastQuality) -> Option<&SimulcastReceiver> {
        let prev_quality = match current {
            SimulcastQuality::High => Some(SimulcastQuality::Medium),
            SimulcastQuality::Medium => Some(SimulcastQuality::Low),
            SimulcastQuality::Low => Some(SimulcastQuality::Undefined),
            SimulcastQuality::Undefined => None,
        };

        if let Some(prev) = prev_quality {
            self.simulcast.iter().find(|s| s.quality == prev)
        } else {
            None
        }
    }

    pub fn lowest_quality(&self) -> &SimulcastReceiver {
        self.simulcast
            .iter()
            .min_by_key(|s| s.quality)
            .expect("no lowest quality, there must be at least 1 layer for TrackReceiver to exist")
    }
}

pub fn new(mid: Mid, meta: Arc<TrackMeta>, capacity: usize) -> (TrackSender, TrackReceiver) {
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

        // TODO: get this from SDP instead
        let clock_rate = match meta.kind {
            MediaKind::Audio => rtp::AUDIO_FREQUENCY,
            MediaKind::Video => rtp::VIDEO_FREQUENCY,
        };
        let (quality, bitrate) = match (meta.kind, rid) {
            (MediaKind::Audio, _) => (SimulcastQuality::Undefined, 64_000),
            (MediaKind::Video, None) => (SimulcastQuality::Undefined, 500_000),
            (MediaKind::Video, Some(r)) if r.starts_with('f') => (SimulcastQuality::High, 800_000),
            (MediaKind::Video, Some(r)) if r.starts_with('h') => {
                (SimulcastQuality::Medium, 300_000)
            }
            (MediaKind::Video, Some(r)) if r.starts_with('q') => (SimulcastQuality::Low, 150_000),
            (MediaKind::Video, Some(rid)) => {
                tracing::warn!("use default bitrate due to unsupported rid: {rid}");
                (SimulcastQuality::Undefined, 500_000)
            }
        };
        let stream_state = StreamState::new(true, bitrate);
        let stream_id = format!("{}:{}", meta.id, rid.as_deref().unwrap_or("_"));
        let monitor = StreamMonitor::new(stream_id, stream_state.clone());

        senders.push(SimulcastSender {
            mid,
            rid,
            quality,
            synchronizer: Synchronizer::new(clock_rate),
            channel: tx,
            keyframe_requests: keyframe_rx,
            keyframe_request_state: KeyframeRequestState::default(),
            keyframe_debounce_duration: Duration::from_millis(300),
            monitor,
        });
        receivers.push(SimulcastReceiver {
            meta: meta.clone(),
            quality,
            rid,
            channel: rx,
            keyframe_requester: keyframe_tx,
            state: stream_state,
        });
    }
    senders.sort_by_key(|e| std::cmp::Reverse(e.quality));
    receivers.sort_by_key(|e| std::cmp::Reverse(e.quality));

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
