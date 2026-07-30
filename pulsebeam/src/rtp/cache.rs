use std::collections::VecDeque;

use crate::rtp::RtpPacket;
use str0m::media::MediaTime;
use str0m::rtp::SeqNo;

const STREAM_CACHE_CAPACITY: usize = 512;

/// How many frames a switch segment may span.
///
/// The keyframe itself is an unavoidable burst — the encoder built it that way.
/// Everything after it is backlog, and releasing backlog in one go reads as
/// queue build-up to the subscriber's congestion control, lowering the estimate
/// exactly when the newly switched layer needs headroom. Past this the segment
/// is refused and the slot waits for the PLI-driven keyframe instead.
const MAX_REPLAY_FRAMES: usize = 3;

/// Ceiling on the packets a switch may release at once when the segment spans
/// more than one frame.
///
/// str0m paces egress, so a burst handed over in one go is not sprayed at the
/// network — it queues, and shows up as delay instead. Either way it is latency
/// the subscriber pays, so the amount released at once is what needs bounding.
const MAX_REPLAY_PACKETS: usize = 96;

/// Absolute ceiling on the packets a switch may release, including lone
/// keyframes.  A 4K IDR can exceed 400 packets; forwarding that many
/// synchronously queues ~600 KB into the pacing layer in one event-loop tick,
/// stalling every other stream on the subscriber's connection for hundreds of
/// milliseconds.  Exceeding this cap returns None so the slot waits for a
/// PLI-driven keyframe — the encoder typically adapts to a smaller size.
const MAX_REPLAY_PACKETS_HARD: usize = 200;

/// Ceiling on the media a switch segment may straddle, in milliseconds.
///
/// This is not a freshness test — how long ago a segment arrived costs nothing.
/// It bounds latency: the output clock lands ahead of real time by whatever the
/// segment spans, and stays there. Loose enough for screen share, where a few
/// frames a second means one frame interval is already a fifth of a second, and
/// tight enough that no single switch can put the subscriber a noticeable
/// distance behind live.
const MAX_REPLAY_SPAN_MS: u64 = 400;

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
            self.open_segment(frame_ts);
        }

        while self.packets.len() > STREAM_CACHE_CAPACITY {
            let evicted = self.packets.pop_front();
            // Losing the head of the segment makes what remains start mid-frame.
            if let (Some(evicted), Some(segment_ts)) = (evicted, self.segment_ts)
                && evicted.rtp_ts.numer() == segment_ts
            {
                self.segment_ts = None;
            }
        }
    }

    /// Anchor the segment at the earliest buffered packet belonging to the
    /// keyframe's frame, so parameter-set packets that arrived just ahead of the
    /// IDR are kept rather than trimmed away.
    fn open_segment(&mut self, frame_ts: u64) {
        debug_assert!(!self.packets.is_empty(), "open_segment needs the keyframe");
        let start = self
            .packets
            .iter()
            .position(|p| p.rtp_ts.numer() == frame_ts)
            .unwrap_or(self.packets.len() - 1);
        self.packets.drain(..start);
        self.segment_ts = Some(frame_ts);
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
    pub fn replay(&self) -> Option<Vec<RtpPacket>> {
        let segment_ts = self.segment_ts?;
        if self.packets.is_empty() {
            return None;
        }

        let mut segment: Vec<RtpPacket> = self.packets.iter().cloned().collect();
        segment.sort_unstable_by_key(|p| *p.seq_no);
        segment.dedup_by_key(|p| *p.seq_no);

        // Trim at the first sequence discontinuity. The burst is emitted with
        // `rewrite_sequential`, which renumbers it onto contiguous output
        // sequence numbers — that erases an internal gap, so a frame whose marker
        // packet was lost upstream would be followed by the next frame with no
        // discontinuity and read as complete. Everything past a gap is undecodable
        // anyway (a packet is missing), so the burst stops there; the rest arrives
        // on the live cursor, where the gap is preserved.
        if let Some(gap) = segment
            .windows(2)
            .position(|w| *w[1].seq_no != (*w[0].seq_no).wrapping_add(1))
        {
            segment.truncate(gap + 1);
        }

        if !segment.iter().any(|p| p.is_keyframe) {
            return None;
        }

        // A late-arriving keyframe fragment can end up with a higher seq_no than
        // the delta frames that followed it.  After sorting by seq_no the burst
        // would have non-monotonic RTP timestamps (T0, T1, T0), and rewriting it
        // sequentially would produce a backwards timestamp in the egress stream.
        // Refuse the replay; a fresh PLI-driven keyframe will be clean.
        if segment
            .windows(2)
            .any(|w| w[1].rtp_ts.numer() < w[0].rtp_ts.numer())
        {
            return None;
        }

        let mut frames = 0usize;
        let mut seen_ts = None;
        for p in &segment {
            let ts = p.rtp_ts.numer();
            if seen_ts != Some(ts) {
                frames += 1;
                seen_ts = Some(ts);
            }
        }
        if frames > MAX_REPLAY_FRAMES {
            return None;
        }
        // Hard per-burst cap — protects against forwarding-latency spikes even
        // for lone keyframes.  PLI requests a fresh (often smaller) keyframe.
        if segment.len() > MAX_REPLAY_PACKETS_HARD {
            return None;
        }
        if frames > 1 && segment.len() > MAX_REPLAY_PACKETS {
            return None;
        }

        let clock_rate = segment[0].rtp_ts.frequency().get() as u64;
        let span = seen_ts.unwrap_or(segment_ts).saturating_sub(segment_ts);
        if span > clock_rate * MAX_REPLAY_SPAN_MS / 1000 {
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

    /// Reads the cache from a subscriber's cursor position.
    ///
    /// This is the single source of packet data for a downstream slot — both
    /// the initial switch burst and the ongoing live tail are read through it,
    /// so there is no separate live path that could desync from the cache.
    ///
    /// - `cursor = None`: the subscriber has nothing yet and needs a decodable
    ///   entry point. Returns the full keyframe segment (identical to
    ///   `replay()`), or `None` if switching here right now would not decode.
    /// - `cursor = Some(seq)`: the subscriber is already following this stream.
    ///   Returns every buffered packet past `seq`, ordered and deduplicated.
    ///   No caps or checks apply — these are the incremental live packets, and
    ///   an empty result (nothing new yet) is `Some((vec![], seq))`, not `None`.
    ///
    /// The returned sequence number is the new cursor: the highest sequence
    /// number emitted, which the caller stores and passes back next time.
    pub fn packets_since(&self, cursor: Option<SeqNo>) -> Option<(Vec<RtpPacket>, SeqNo)> {
        match cursor {
            None => {
                let segment = self.replay()?;
                let new_cursor = segment.last()?.seq_no;
                Some((segment, new_cursor))
            }
            Some(cursor) => {
                let mut packets: Vec<RtpPacket> = self
                    .packets
                    .iter()
                    .filter(|p| *p.seq_no > *cursor)
                    .cloned()
                    .collect();
                packets.sort_unstable_by_key(|p| *p.seq_no);
                packets.dedup_by_key(|p| *p.seq_no);
                let new_cursor = packets.last().map_or(cursor, |p| p.seq_no);
                Some((packets, new_cursor))
            }
        }
    }

    /// The buffered packet with this input sequence number, if still cached.
    ///
    /// Used by the tail drain to complete a frame left half-sent by a stream the
    /// slot switched away from: the hole is known, so the packet that fills it is
    /// looked up directly rather than scanned for in arrival order.
    pub fn get(&self, seq: SeqNo) -> Option<&RtpPacket> {
        self.packets.iter().find(|p| p.seq_no == seq)
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
        self.sps = None;
        self.pps = None;
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::rtp::test_utils::{H264StreamBuilder, ParameterSetStyle};
    use std::time::Duration;
    use tokio::time::Instant;

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

        let replay = cache.replay().expect("replayable");
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

        let replay = cache.replay().expect("replayable");
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
        let replay = cache.replay().expect("replayable");
        assert_eq!(replay.len(), kf.len());
    }

    /// Screen share sits still for long stretches: one keyframe, then nothing
    /// until the picture changes. That keyframe is still exactly what the
    /// subscriber should see, and replaying it costs nothing — it is a single
    /// frame, so there is no backlog to burst and no span to jump the clock
    /// over. Refusing it leaves the viewer on a black screen until a PLI round
    /// trip produces a duplicate of the frame already in hand.
    #[test]
    fn a_long_idle_keyframe_is_still_replayable() {
        let mut b = builder(ParameterSetStyle::SeparatePacket);
        let mut cache = StreamCache::new();
        let kf = b.keyframe(4);
        for p in &kf {
            cache.push(p);
        }
        let arrived = kf.last().unwrap().arrival_ts;

        let much_later = arrived + Duration::from_secs(30);
        assert!(
            cache.replay().is_some(),
            "a still screen must not be withheld just because it has been still"
        );
    }

    /// A segment straddling a long idle gap is refused: replaying both sides
    /// would jump the output clock forward by the length of the gap.
    #[test]
    fn a_segment_straddling_a_long_gap_is_not_replayable() {
        // One frame per second: three frames is two seconds of media.
        let mut b = builder(ParameterSetStyle::SeparatePacket).with_fps(1);
        let mut cache = StreamCache::new();
        for p in b.keyframe(2) {
            cache.push(&p);
        }
        assert!(cache.replay().is_some(), "the keyframe alone is free");

        for p in b.delta_frames(2, 1) {
            cache.push(&p);
        }
        assert!(
            cache.replay().is_none(),
            "a segment spanning seconds of media would jump the output clock"
        );
    }

    /// A lone keyframe larger than the per-frame cap but within the hard cap is
    /// still replayable.  The per-frame cap only applies to multi-frame bursts.
    #[test]
    fn a_lone_keyframe_within_the_hard_packet_cap_is_replayable() {
        let mut b = builder(ParameterSetStyle::SeparatePacket);
        let mut cache = StreamCache::new();
        // MAX_REPLAY_PACKETS + 10 sits above the per-frame cap (96) while well
        // below the hard cap (200), even accounting for SPS+PPS overhead (~2 pkts).
        let kf = b.keyframe(MAX_REPLAY_PACKETS + 10);
        assert!(kf.len() > MAX_REPLAY_PACKETS, "fixture must exceed the per-frame cap");
        assert!(
            kf.len() <= MAX_REPLAY_PACKETS_HARD,
            "fixture must not exceed the hard cap"
        );
        for p in &kf {
            cache.push(p);
        }
        assert!(
            cache.replay().is_some(),
            "a lone keyframe within the hard cap must be replayable"
        );
    }

    /// A lone keyframe past the hard cap is refused to bound the forwarding
    /// burst and prevent P99 latency spikes on the subscriber's connection.
    #[test]
    fn a_lone_keyframe_past_the_hard_packet_cap_is_refused() {
        let mut b = builder(ParameterSetStyle::SeparatePacket);
        let mut cache = StreamCache::new();
        // MAX_REPLAY_PACKETS_HARD + 10 guarantees the segment exceeds the hard
        // cap even after SPS+PPS overhead is included.
        let kf = b.keyframe(MAX_REPLAY_PACKETS_HARD + 10);
        assert!(
            kf.len() > MAX_REPLAY_PACKETS_HARD,
            "fixture must exceed the hard cap"
        );
        for p in &kf {
            cache.push(p);
        }
        assert!(
            cache.replay().is_none(),
            "a lone keyframe exceeding the hard cap must be refused to prevent latency spikes"
        );
    }

    /// Once there is more than the entry point, the packet ceiling applies —
    /// there is a smaller segment to be had by asking for a fresh keyframe.
    #[test]
    fn a_large_segment_past_the_keyframe_is_refused() {
        let mut b = builder(ParameterSetStyle::SeparatePacket);
        let mut cache = StreamCache::new();
        for p in b.keyframe(MAX_REPLAY_PACKETS) {
            cache.push(&p);
        }
        assert!(cache.replay().is_some());

        for p in b.delta_frame(4) {
            cache.push(&p);
        }
        assert!(
            cache.replay().is_none(),
            "a segment past the entry point must respect the packet ceiling"
        );
    }

    /// Screen share runs at a few frames a second. A couple of its frames is a
    /// normal segment and must not be mistaken for a backlog.
    #[test]
    fn ordinary_screen_share_pacing_is_replayable() {
        let mut b = builder(ParameterSetStyle::SeparatePacket).with_fps(5);
        let mut cache = StreamCache::new();
        for p in b.keyframe(3) {
            cache.push(&p);
        }
        for p in b.delta_frame(1) {
            cache.push(&p);
        }
        assert!(
            cache.replay().is_some(),
            "a 5fps screen share must switch from cache, not fall back to a keyframe request"
        );
    }

    #[test]
    fn packets_since_none_returns_the_full_burst_and_its_last_seq_as_cursor() {
        let mut b = builder(ParameterSetStyle::SeparatePacket);
        let mut cache = StreamCache::new();
        for p in b.keyframe(4) {
            cache.push(&p);
        }

        let (burst, cursor) = cache.packets_since(None).expect("replayable");
        let replay = cache.replay().expect("replayable");
        assert_eq!(burst.len(), replay.len(), "None cursor mirrors replay()");
        assert_eq!(
            cursor,
            burst.last().unwrap().seq_no,
            "cursor is the highest seq in the burst"
        );
    }

    #[test]
    fn packets_since_a_cursor_returns_only_newer_packets_ordered() {
        let mut b = builder(ParameterSetStyle::SeparatePacket);
        let mut cache = StreamCache::new();
        for p in b.keyframe(2) {
            cache.push(&p);
        }
        let (burst, cursor) = cache.packets_since(None).expect("replayable");

        // Nothing new yet: empty batch, cursor unchanged.
        let (empty, same) = cache.packets_since(Some(cursor)).expect("live read");
        assert!(empty.is_empty(), "no packets past the cursor yet");
        assert_eq!(same, cursor, "cursor holds when nothing is new");

        // A live delta frame arrives.
        let delta = b.delta_frame(2);
        for p in &delta {
            cache.push(p);
        }
        let (live, advanced) = cache.packets_since(Some(cursor)).expect("live read");
        assert!(
            live.iter().all(|p| *p.seq_no > *cursor),
            "only packets past the cursor are returned"
        );
        assert!(
            live.windows(2).all(|w| *w[0].seq_no < *w[1].seq_no),
            "live packets are ordered by sequence number"
        );
        assert_eq!(
            advanced,
            live.last().unwrap().seq_no,
            "cursor advances to the newest packet"
        );
        assert!(
            *advanced > *cursor,
            "cursor moved forward past the delta frame"
        );
        // The burst's own packets are never redelivered on a live read.
        let burst_max = *burst.last().unwrap().seq_no;
        assert!(
            live.iter().all(|p| *p.seq_no > burst_max),
            "packets already in the burst are not returned again"
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
        assert!(cache.replay().is_none());
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

        let replay = cache.replay().expect("replayable");
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
        assert!(cache.replay().is_none());
    }

    /// A keyframe fragment that arrives after delta frames (due to network
    /// reordering) produces a burst with non-monotonic timestamps in seq_no
    /// order.  The egress switcher would output backwards RTP timestamps when
    /// replaying such a burst, triggering the stream invariant violation.
    /// The cache must refuse the replay and wait for a clean keyframe.
    #[test]
    fn replay_refused_when_late_idr_fragment_creates_non_monotonic_timestamps() {
        let mut b = builder(ParameterSetStyle::SeparatePacket);
        let mut cache = StreamCache::new();

        // Normal keyframe: SPS/PPS + 2 IDR frags (seq 1000-1002, ts=T0).
        let kf = b.keyframe(2);
        let kf_ts = kf[0].rtp_ts.numer();
        for p in &kf {
            cache.push(p);
        }
        assert!(cache.replay().is_some(), "clean keyframe is replayable");

        // Delta frame (seq 1003-1003, ts=T1 > T0).
        let delta = b.delta_frame(1);
        let delta_ts = delta[0].rtp_ts.numer();
        assert!(delta_ts > kf_ts, "delta must have a higher timestamp");
        for p in &delta {
            cache.push(p);
        }

        // Simulate a late IDR fragment: same ts as keyframe (T0) but
        // seq_no higher than the delta packets (so it sorts last in the burst).
        let delta_last_seq = *delta.last().unwrap().seq_no;
        let mut late_frag = kf.last().unwrap().clone();
        late_frag.seq_no = (delta_last_seq + 1).into();
        // seq = delta_last_seq+1 is still within the keyframe's segment (same ts=T0).
        cache.push(&late_frag);

        // The burst is now [kf_seqs..., delta_seqs..., late_frag_seq] sorted by
        // seq_no, with timestamps [T0, ..., T1, ..., T0] — non-monotonic.
        assert!(
            cache.replay().is_none(),
            "a burst with non-monotonic timestamps must be refused to prevent egress violation"
        );
    }
}
