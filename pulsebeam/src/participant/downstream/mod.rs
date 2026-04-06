mod audio;
mod video;

use crate::audio_selector::AudioSelectorSubscription;
use crate::entity::{ParticipantId, TrackId};
use crate::participant::downstream::audio::AudioAllocator;
use crate::participant::downstream::video::{SlotConfig, VideoAllocator};
use crate::rtp::RtpPacket;
use crate::track::{StreamId, TrackMeta, TrackReceiver};
use ahash::{HashMap, HashMapExt};
use futures_util::Stream;
use std::pin::Pin;
use std::task::{Context, Poll};
use str0m::bwe::{Bitrate, Bwe};
use str0m::media::{KeyframeRequest, MediaAdded, MediaKind, Mid, Pt};
use str0m::rtp::Ssrc;
use tokio::time::Instant;
pub use video::Intent;

const MIN_BANDWIDTH: Bitrate = Bitrate::kbps(300);
const MAX_BANDWIDTH: Bitrate = Bitrate::mbps(5);

#[derive(Clone)]
pub struct Slot {
    pub mid: Mid,
    pub ssrc: Ssrc,
    pub pt: Pt,
}

pub struct DownstreamAllocator {
    pub dirty_allocation: bool,
    available_bandwidth: Bitrate,
    video_tracks: HashMap<TrackId, TrackMeta>,
    video_slots: Vec<Slot>,
    pub video: VideoAllocator,

    routing: HashMap<StreamId, Slot>,
}

impl DownstreamAllocator {
    pub fn new(participant_id: ParticipantId, manual_sub: bool) -> Self {
        Self {
            available_bandwidth: MIN_BANDWIDTH,
            video_tracks: HashMap::new(),
            video_slots: Vec::new(),
            video: VideoAllocator::new(manual_sub),
            routing: HashMap::new(),
            dirty_allocation: false,
        }
    }

    pub fn add_track(&mut self, track: TrackMeta) {
        if track.kind.is_audio() {
            return;
        }

        self.video_tracks.insert(track.id, track);
    }

    pub fn add_slot(&mut self, slot: Slot) {
        self.video_slots.push(slot);
    }

    pub fn update_bitrate(&mut self, available_bandwidth: Bitrate) {
        self.available_bandwidth = available_bandwidth.max(MIN_BANDWIDTH).min(MAX_BANDWIDTH);
        self.dirty_allocation = true;
    }

    pub fn update_allocations(&mut self, bwe: &mut Bwe) {
        self.dirty_allocation = false;
        let desired = self.video.update_allocations(self.available_bandwidth);
        bwe.set_desired_bitrate(desired);
    }

    pub fn poll_slow(&mut self, now: Instant, bwe: &mut Bwe) {
        // TODO: super hacky allocation
        for slot in &self.video_slots {
            let Some(track) = self.video_tracks.iter().next() else {
                continue;
            };

            let rid = track.1.simulcast_rids.first();
            let stream_id: StreamId = (*track.0, rid.copied());
            self.routing.insert(stream_id, slot.clone());
        }
    }

    pub fn on_forward_rtp(&self, stream_id: &StreamId, pkt: &RtpPacket, rtc: &mut str0m::Rtc) {
        // TODO: hack
        let Some(slot) = self.routing.get(stream_id) else {
            return;
        };

        let mut api = rtc.direct_api();
        let Some(writer) = api.stream_tx(&slot.ssrc) else {
            tracing::warn!(%slot.ssrc, "Dropping RTP for invalid stream mid");
            return;
        };

        tracing::trace!(
            "forward rtp: seqno={},rtp_ts={:?},playout_time={:?}",
            pkt.seq_no,
            pkt.rtp_ts,
            pkt.playout_time
        );
        if let Err(err) = writer.write_rtp(
            slot.pt,
            pkt.seq_no,
            pkt.rtp_ts.numer() as u32,
            pkt.playout_time.into(),
            pkt.marker,
            pkt.ext_vals.clone(),
            true,
            pkt.payload.to_vec(),
        ) {
            tracing::warn!(%slot.ssrc, "Dropping RTP for invalid rtp header: {err:?}");
        }
    }

    pub fn handle_keyframe_request(&mut self, req: KeyframeRequest) {}
}
