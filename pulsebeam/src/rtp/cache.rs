use std::collections::VecDeque;
use std::time::Duration;

use crate::rtp::RtpPacket;
use str0m::media::MediaTime;
use tokio::time::Instant;

const STREAM_CACHE_CAPACITY: usize = 512;

/// A cached segment older than this is not worth replaying. Bursting a stale
/// segment at the subscriber costs a congestion spike and pushes the output RTP
/// clock forward by the segment's whole duration. Past this age the slot keeps
/// forwarding its current layer and waits for the PLI-driven keyframe instead.
pub const MAX_REPLAY_AGE: Duration = Duration::from_millis(200);

/// Per-stream ring buffer seeded from the last keyframe onward.
///
/// Shared across all subscribers of the same upstream stream. A new subscriber
/// can replay the cached segment for an instant switch instead of waiting for a
/// PLI round-trip.
///
/// The segment is delimited by RTP timestamp, not arrival time, so it always
/// covers a whole frame: the parameter sets that precede an IDR share its RTP
/// timestamp and must travel with it, and a multi-slice keyframe reports an IDR
/// on more than one packet without starting a new frame.
#[derive(Debug)]
pub struct StreamCache {
    /// Recent packets. Once a keyframe is seen, the front is trimmed to that
    /// keyframe frame's first packet, so this doubles as the replay segment.
    packets: VecDeque<RtpPacket>,
    /// RTP timestamp of the keyframe frame the current segment starts at.
    segment_ts: Option<u64>,
    /// Arrival time of the keyframe that opened the current segment.
    segment_at: Option<Instant>,
    /// Most recent packets carrying SPS / PPS, retained independently of the
    /// segment so a switch can be prefixed with parameter sets even when the
    /// encoder only emits them once at stream start.
    sps: Option<RtpPacket>,
    pps: Option<RtpPacket>,
}

impl Default for StreamCache {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamCache {
    pub fn new() -> Self {
        Self {
            packets: VecDeque::new(),
            segment_ts: None,
            segment_at: None,
            sps: None,
            pps: None,
        }
    }

    pub fn push(&mut self, pkt: &RtpPacket) {
        if pkt.nal.sps() {
            self.sps = Some(pkt.clone());
        }
        if pkt.nal.pps() {
            self.pps = Some(pkt.clone());
        }

        self.packets.push_back(pkt.clone());

        let frame_ts = pkt.rtp_ts.numer();
        if pkt.is_keyframe && self.segment_ts != Some(frame_ts) {
            self.open_segment(frame_ts, pkt.arrival_ts);
        }

        while self.packets.len() > STREAM_CACHE_CAPACITY {
            let evicted = self.packets.pop_front();
            // Losing the head of the segment makes what remains start mid-frame.
            if let (Some(evicted), Some(segment_ts)) = (evicted, self.segment_ts)
                && evicted.rtp_ts.numer() == segment_ts
            {
                self.segment_ts = None;
                self.segment_at = None;
            }
        }
    }

    /// Anchor the segment at the earliest buffered packet belonging to the
    /// keyframe's frame, so parameter-set packets that arrived just ahead of the
    /// IDR are kept rather than trimmed away.
    fn open_segment(&mut self, frame_ts: u64, at: Instant) {
        debug_assert!(!self.packets.is_empty(), "open_segment needs the keyframe");
        let start = self
            .packets
            .iter()
            .position(|p| p.rtp_ts.numer() == frame_ts)
            .unwrap_or(self.packets.len() - 1);
        self.packets.drain(..start);
        self.segment_ts = Some(frame_ts);
        self.segment_at = Some(at);
        debug_assert_eq!(
            self.packets.front().map(|p| p.rtp_ts.numer()),
            Some(frame_ts),
            "segment must start at the keyframe frame"
        );
    }

    pub fn has_keyframe(&self) -> bool {
        self.segment_ts.is_some()
    }

    /// Returns a switch-ready snapshot of the current keyframe segment, or
    /// `None` if switching to this stream right now would not be decodable.
    ///
    /// The result is ordered by sequence number, deduplicated, and guaranteed to
    /// carry SPS, PPS and an IDR — the subscriber has no jitter buffer ahead of
    /// it and the egress SSRC is shared across layers, so it cannot reorder the
    /// burst itself nor fall back on parameter sets from the previous layer.
    pub fn replay(&self, now: Instant) -> Option<Vec<RtpPacket>> {
        let segment_ts = self.segment_ts?;
        let segment_at = self.segment_at?;

        if now.saturating_duration_since(segment_at) > MAX_REPLAY_AGE {
            return None;
        }
        if self.packets.is_empty() {
            return None;
        }

        let mut segment: Vec<RtpPacket> = self.packets.iter().cloned().collect();
        segment.sort_unstable_by_key(|p| *p.seq_no);
        segment.dedup_by_key(|p| *p.seq_no);

        if !segment.iter().any(|p| p.is_keyframe) {
            return None;
        }

        let mut out = self.parameter_set_prefix(&segment, segment_ts)?;
        out.extend(segment);

        debug_assert!(out.iter().any(|p| p.nal.sps()), "replay lacks SPS");
        debug_assert!(out.iter().any(|p| p.nal.pps()), "replay lacks PPS");
        debug_assert!(out.iter().any(|p| p.is_keyframe), "replay lacks a keyframe");
        debug_assert!(
            out.windows(2).all(|w| *w[0].seq_no <= *w[1].seq_no),
            "replay must be ordered by sequence number"
        );
        Some(out)
    }

    /// Parameter-set packets the segment is missing, restamped onto the
    /// keyframe's frame so they do not read as a separate, earlier frame.
    fn parameter_set_prefix(
        &self,
        segment: &[RtpPacket],
        segment_ts: u64,
    ) -> Option<Vec<RtpPacket>> {
        let needs_sps = !segment.iter().any(|p| p.nal.sps());
        let needs_pps = !segment.iter().any(|p| p.nal.pps());
        if !needs_sps && !needs_pps {
            return Some(Vec::new());
        }

        let anchor = segment.first()?;
        let mut prefix: Vec<RtpPacket> = Vec::with_capacity(2);
        if needs_sps {
            prefix.push(self.sps.clone()?);
        }
        if needs_pps && !prefix.iter().any(|p| p.nal.pps()) {
            let pps = self.pps.clone()?;
            if !prefix
                .iter()
                .any(|p| p.ssrc == pps.ssrc && p.seq_no == pps.seq_no)
            {
                prefix.push(pps);
            }
        }

        let n = prefix.len();
        for (i, p) in prefix.iter_mut().enumerate() {
            p.rtp_ts = MediaTime::new(segment_ts, p.rtp_ts.frequency());
            p.arrival_ts = anchor.arrival_ts;
            p.playout_time = anchor.playout_time;
            p.marker = false;
            // Keep the prefix ahead of the segment once it is sorted by seq.
            p.seq_no = (*anchor.seq_no).saturating_sub((n - i) as u64).into();
        }

        Some(prefix)
    }

    pub fn clear(&mut self) {
        self.packets.clear();
        self.segment_ts = None;
        self.segment_at = None;
        self.sps = None;
        self.pps = None;
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::rtp::test_utils::{H264StreamBuilder, ParameterSetStyle};

    fn builder(style: ParameterSetStyle) -> H264StreamBuilder {
        H264StreamBuilder::new(1, 1000, 90_000, Instant::now()).with_parameter_sets(style)
    }

    #[test]
    fn replay_keeps_the_parameter_sets_that_precede_the_idr() {
        let mut b = builder(ParameterSetStyle::SeparatePacket);
        let mut cache = StreamCache::new();
        for p in b.keyframe(4) {
            cache.push(&p);
        }
        let last = b.delta_frame(1);
        for p in &last {
            cache.push(p);
        }

        let replay = cache.replay(last[0].arrival_ts).expect("replayable");
        assert!(replay.iter().any(|p| p.nal.sps()));
        assert!(replay.iter().any(|p| p.nal.pps()));
        assert!(replay.iter().any(|p| p.is_keyframe));
    }

    #[test]
    fn replay_synthesizes_parameter_sets_the_encoder_only_sent_once() {
        let mut b = builder(ParameterSetStyle::OnceAtStreamStart);
        let mut cache = StreamCache::new();
        for p in b.keyframe(2) {
            cache.push(&p);
        }
        for p in b.delta_frames(3, 2) {
            cache.push(&p);
        }
        // A later IDR with no parameter sets of its own.
        let kf = b.keyframe(2);
        for p in &kf {
            cache.push(p);
        }

        let replay = cache
            .replay(kf.last().unwrap().arrival_ts)
            .expect("replayable");
        assert!(replay.iter().any(|p| p.nal.sps()));
        assert!(replay.iter().any(|p| p.nal.pps()));
        assert_eq!(
            replay.iter().map(|p| p.rtp_ts.numer()).min(),
            Some(kf[0].rtp_ts.numer()),
            "restamped parameter sets must not read as an earlier frame"
        );
    }

    #[test]
    fn a_multi_slice_keyframe_opens_exactly_one_segment() {
        let mut b = builder(ParameterSetStyle::SeparatePacket);
        let mut cache = StreamCache::new();
        let kf = b.keyframe_with_slices(4, 2);
        for p in &kf {
            cache.push(p);
        }
        let replay = cache
            .replay(kf.last().unwrap().arrival_ts)
            .expect("replayable");
        assert_eq!(replay.len(), kf.len());
    }

    #[test]
    fn a_stale_segment_is_not_replayable() {
        let mut b = builder(ParameterSetStyle::SeparatePacket);
        let mut cache = StreamCache::new();
        let kf = b.keyframe(2);
        for p in &kf {
            cache.push(p);
        }
        let at = kf.last().unwrap().arrival_ts;
        assert!(cache.replay(at).is_some());
        assert!(
            cache
                .replay(at + MAX_REPLAY_AGE + Duration::from_millis(1))
                .is_none(),
            "a stale GOP must not be burst at the subscriber"
        );
    }

    #[test]
    fn a_stream_without_a_keyframe_is_never_replayable() {
        let mut b = builder(ParameterSetStyle::SeparatePacket);
        let mut cache = StreamCache::new();
        let frames = b.delta_frames(5, 2);
        for p in &frames {
            cache.push(p);
        }
        assert!(!cache.has_keyframe());
        assert!(cache.replay(frames.last().unwrap().arrival_ts).is_none());
    }

    #[test]
    fn replay_is_ordered_and_deduplicated_under_reordering() {
        let mut b = builder(ParameterSetStyle::SeparatePacket);
        let mut cache = StreamCache::new();
        let mut kf = b.keyframe(4);
        kf.swap(0, 2);
        let dup = kf[1].clone();
        for p in kf.iter().chain(std::iter::once(&dup)) {
            cache.push(p);
        }

        let replay = cache
            .replay(kf.last().unwrap().arrival_ts)
            .expect("replayable");
        assert!(replay.windows(2).all(|w| *w[0].seq_no < *w[1].seq_no));
        assert_eq!(replay.len(), kf.len());
    }

    #[test]
    fn evicting_the_segment_head_invalidates_the_segment() {
        let mut b = builder(ParameterSetStyle::SeparatePacket);
        let mut cache = StreamCache::new();
        for p in b.keyframe(2) {
            cache.push(&p);
        }
        assert!(cache.has_keyframe());

        let flood = b.delta_frames(STREAM_CACHE_CAPACITY, 2);
        for p in &flood {
            cache.push(p);
        }
        assert!(!cache.has_keyframe(), "an over-long GOP is not switchable");
        assert!(cache.replay(flood.last().unwrap().arrival_ts).is_none());
    }
}
