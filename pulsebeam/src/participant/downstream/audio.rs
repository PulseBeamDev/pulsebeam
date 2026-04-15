use std::time::Duration;

use ahash::{HashMap, HashMapExt};
use tokio::time::Instant;

use crate::rtp;
use crate::rtp::RtpPacket;
use crate::rtp::timeline::Timeline;
use crate::track::StreamId;
use str0m::media::{Mid, Pt};
use str0m::rtp::Ssrc;

/// Playout-time gap that indicates a DTX/VAD silence period has ended.
/// Two Opus frames (2 × 20 ms) is enough headroom to distinguish a genuine
/// talkspurt break from routine jitter on back-to-back speech packets.
const TALKSPURT_GAP: Duration = Duration::from_millis(40);

pub struct AudioAllocator {
    slots: Vec<AudioSlot>,
    stream_to_slot: HashMap<StreamId, usize>,
}

impl AudioAllocator {
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            stream_to_slot: HashMap::new(),
        }
    }

    pub fn add_slot(&mut self, mid: Mid, pt: Pt, ssrc: Ssrc) {
        self.slots.push(AudioSlot::new(mid, pt, ssrc));
    }

    #[inline]
    pub fn on_rtp(
        &mut self,
        stream_id: &StreamId,
        pkt: RtpPacket,
        _now: Instant,
    ) -> Option<(Mid, Pt, Ssrc, RtpPacket)> {
        let idx = if let Some(&idx) = self.stream_to_slot.get(stream_id) {
            idx
        } else {
            // The shard-level TopNAudioSelector guarantees at most
            // MAX_SEND_AUDIO_SLOTS streams are delivered here, which equals
            // the number of slots provisioned, so a free slot is always
            // available for a newly arrived stream.
            let target = self.slots.iter().position(|s| s.owner.is_none())?;
            self.slots[target].owner = Some(*stream_id);
            self.slots[target].do_rebase(&pkt);
            self.stream_to_slot.insert(*stream_id, target);
            target
        };

        self.slots[idx].process(pkt)
    }

    pub fn remove_stream(&mut self, stream_id: &StreamId) {
        if let Some(idx) = self.stream_to_slot.remove(stream_id) {
            self.slots[idx].owner = None;
        }
    }
}

struct AudioSlot {
    mid: Mid,
    pt: Pt,
    ssrc: Ssrc,
    owner: Option<StreamId>,
    timeline: Timeline,
    last_ssrc: Option<Ssrc>,
    /// Input seq_no (mod 2^16) of the first packet seen after the most recent
    /// stream switch.  Any packet whose input seq_no is strictly before this
    /// value (wrapping) is dropped — it was in-flight before the switch and
    /// would corrupt the receiver's decoder state.
    switch_seq: u16,
    /// When true, the next forwarded packet has its marker bit set to signal
    /// a talk-spurt start to the receiver.
    pending_marker: bool,
    /// Playout time of the last forwarded packet from this slot's current stream.
    /// Packets with an earlier playout_time are late arrivals whose timestamps
    /// would regress in the output — the subscriber JB would discard them anyway,
    /// so we drop them here to keep the output strictly non-decreasing.
    /// Reset on every stream switch so a new stream can start from a clean baseline.
    last_playout: Option<Instant>,
}

impl AudioSlot {
    fn new(mid: Mid, pt: Pt, ssrc: Ssrc) -> Self {
        Self {
            mid,
            pt,
            ssrc,
            owner: None,
            timeline: Timeline::new(rtp::AUDIO_FREQUENCY),
            last_ssrc: None,
            switch_seq: 0,
            pending_marker: false,
            last_playout: None,
        }
    }

    /// Rebase the timeline to `pkt` and arm the per-switch guard state.
    /// Called both on slot ownership change and on mid-stream SSRC changes.
    fn do_rebase(&mut self, pkt: &RtpPacket) {
        self.timeline.rebase_audio(pkt);
        self.switch_seq = *pkt.seq_no as u16;
        self.pending_marker = true;
        self.last_ssrc = Some(pkt.ssrc);
        // Reset the monotonicity guard: the new stream has its own playout timeline.
        self.last_playout = None;
    }

    fn process(&mut self, mut pkt: RtpPacket) -> Option<(Mid, Pt, Ssrc, RtpPacket)> {
        // Rebase on mid-stream SSRC change (rare — e.g. ICE restart on publisher).
        if self.last_ssrc != Some(pkt.ssrc) {
            self.do_rebase(&pkt);
        }

        // Drop packets that arrived out-of-order from before the switch point.
        // Uses the standard RFC 3550 half-window test for wrapping seq_no.
        let seq16 = *pkt.seq_no as u16;
        if !seq_is_after_or_equal(seq16, self.switch_seq) {
            return None;
        }

        // Drop late-arriving packets that would cause a playout_time regression.
        // Such packets missed their playback deadline; the subscriber JB would
        // discard them anyway.
        if let Some(last) = self.last_playout {
            if pkt.playout_time < last {
                return None;
            }
            // A gap larger than TALKSPURT_GAP means silence ended and speech
            // resumed (VAD filtered the silence run).  Signal this to the
            // subscriber JB with a marker bit so it resets its delay target
            // rather than treating the timestamp jump as ongoing network loss.
            if pkt.playout_time.duration_since(last) >= TALKSPURT_GAP {
                self.pending_marker = true;
            }
        }
        self.last_playout = Some(pkt.playout_time);

        // First packet after a switch or talkspurt resumption.
        if self.pending_marker {
            pkt.marker = true;
            self.pending_marker = false;
        }

        pkt = self.timeline.rewrite(pkt);
        Some((self.mid, self.pt, self.ssrc, pkt))
    }
}

/// Returns true if `seq` is at or after `reference` in the wrapping u16 space.
/// Uses the RFC 3550 half-window convention: forward distance < 2^15.
#[inline]
fn seq_is_after_or_equal(seq: u16, reference: u16) -> bool {
    seq.wrapping_sub(reference) < 0x8000
}
