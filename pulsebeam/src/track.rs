use std::{sync::Arc, time::Duration};

use pulsebeam_runtime::sync::spmc;
use str0m::media::{KeyframeRequestKind, MediaKind, Rid};
use tokio::sync::watch;
use tokio::time::Instant;

use crate::rtp::{
    RtpPacket,
    jitter_buffer::{self, PollResult},
    monitor::{HealthThresholds, StreamMonitor, StreamState},
};

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
    pub state: StreamState,
}

impl SimulcastReceiver {
    pub fn request_keyframe(&self, kind: KeyframeRequestKind) {
        let request = str0m::media::KeyframeRequest {
            mid: self.meta.id.origin_mid,
            rid: self.rid,
            kind,
        };
        let wrapped = KeyframeRequest {
            request,
            requested_at: Instant::now(),
        };
        if self.keyframe_requester.send(Some(wrapped)).is_err() {
            tracing::warn!(?request, "feedback channel is unavailable");
        }
    }
}

#[derive(Debug)]
pub struct SimulcastSender {
    pub rid: Option<Rid>,
    pub keyframe_requests: watch::Receiver<Option<KeyframeRequest>>,
    pub last_keyframe_requested_at: Option<Instant>,
    pub monitor: StreamMonitor,
    channel: spmc::Sender<RtpPacket>,
    jb: jitter_buffer::JitterBuffer<RtpPacket>,
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
        // self.jb.push(pkt)
        self.forward(pkt);
    }

    pub fn poll(&mut self, now: Instant) -> Option<Instant> {
        self.monitor.poll(now);
        loop {
            match self.jb.poll(now) {
                PollResult::PacketReady(pkt) => self.forward(pkt),
                PollResult::WaitUntil(deadline) => return Some(deadline),
                PollResult::Empty => return None,
            }
        }
    }

    fn forward(&mut self, pkt: RtpPacket) {
        self.monitor
            .process_packet(&pkt, pkt.payload.len() + pkt.header.header_len);
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

    pub fn higher_quality(&self, rid: &Option<Rid>) -> Option<&SimulcastReceiver> {
        let idx = self.simulcast.iter().position(|s| s.rid == *rid)?;
        self.simulcast.get(idx.saturating_sub(1))
    }

    pub fn lower_quality(&self, rid: &Option<Rid>) -> Option<&SimulcastReceiver> {
        let idx = self.simulcast.iter().position(|s| s.rid == *rid)?;
        self.simulcast.get(idx.saturating_add(1))
    }

    pub fn lowest_quality(&self) -> &SimulcastReceiver {
        self.simulcast
            .last()
            .expect("no lowest quality, there must be at least 1 layer for TrackReceiver to exist")
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
        let stream_state = StreamState::new(true, bitrate);
        let monitor = StreamMonitor::new(stream_state.clone(), HealthThresholds::default());
        let jbc = if meta.kind == MediaKind::Video {
            jitter_buffer::JitterBufferConfig::video_interactive()
        } else {
            jitter_buffer::JitterBufferConfig::audio_interactive()
        };

        senders.push(SimulcastSender {
            rid,
            channel: tx,
            keyframe_requests: keyframe_rx,
            last_keyframe_requested_at: None,
            monitor,
            jb: jitter_buffer::JitterBuffer::new(jbc),
        });
        receivers.push(SimulcastReceiver {
            meta: meta.clone(),
            rid,
            channel: rx,
            keyframe_requester: keyframe_tx,
            state: stream_state,
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
