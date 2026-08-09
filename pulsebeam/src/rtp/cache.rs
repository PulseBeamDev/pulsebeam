//! Shared-state exception: Test-only: `Arc::new` builds header extensions for fixtures, matching what
//! str0m hands us at ingress.
#![allow(clippy::disallowed_types)]

use crate::rtp::RtpPacket;
use str0m::media::{MediaTime, Rid};
use str0m::rtp::SeqNo;

/// Ring capacity. Must be a power of two so `seq & CACHE_MASK` indexes a slot.
const STREAM_CACHE_CAPACITY: usize = 512;
const CACHE_MASK: u64 = (STREAM_CACHE_CAPACITY - 1) as u64;
const _: () = assert!(STREAM_CACHE_CAPACITY.is_power_of_two());

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
    /// Direct-mapped ring indexed by `seq & CACHE_MASK`. Two sequence numbers
    /// `CAPACITY` apart share a slot; a read verifies `slot.seq_no == wanted`, so
    /// an evicted entry (overwritten by a newer packet at the same slot) reads as
    /// absent.
    ring: Box<[Option<RtpPacket>]>,
    /// Highest sequence number stored — the write frontier. The live window is
    /// `[newest_seq - CAPACITY + 1, newest_seq]`.
    newest_seq: Option<u64>,
    /// First sequence number of the current keyframe frame, and its RTP
    /// timestamp. The replay segment runs from here to the frontier.
    segment_start_seq: Option<u64>,
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
            ring: std::iter::repeat_with(|| None)
                .take(STREAM_CACHE_CAPACITY)
                .collect(),
            newest_seq: None,
            segment_start_seq: None,
            segment_ts: None,
            sps: None,
            pps: None,
        }
    }

    /// The packet occupying `seq`'s slot, if that slot actually holds `seq`.
    #[inline]
    fn slot(&self, seq: u64) -> Option<&RtpPacket> {
        self.ring[(seq & CACHE_MASK) as usize]
            .as_ref()
            .filter(|p| *p.seq_no == seq)
    }

    /// Take ownership of a packet, handing it back only when it was too old to
    /// store.
    ///
    /// By value because the cache holds a copy of every packet regardless, so a
    /// caller that also needs one should read the stored copy rather than make
    /// a second. That is not a micro-optimisation: `ExtensionValues` carries a
    /// type-keyed map, so cloning an `RtpPacket` heap-allocates — measured at
    /// 75ns against 35ns for one with no extensions.
    pub fn push(&mut self, pkt: RtpPacket) -> Option<RtpPacket> {
        if pkt.nal.sps() {
            self.sps = Some(pkt.clone());
        }
        if pkt.nal.pps() {
            self.pps = Some(pkt.clone());
        }

        let seq = *pkt.seq_no;

        // Reject a packet that has already slid out of the window: its slot now
        // holds a newer packet, and overwriting that would corrupt the window.
        if let Some(newest) = self.newest_seq
            && seq.wrapping_add(STREAM_CACHE_CAPACITY as u64) <= newest
        {
            return Some(pkt);
        }

        let frame_ts = pkt.rtp_ts.numer();
        let is_keyframe = pkt.is_keyframe;

        // Placing the packet naturally evicts whatever occupied its slot
        // `CAPACITY` positions ago — no eviction loop needed.
        self.ring[(seq & CACHE_MASK) as usize] = Some(pkt);
        self.newest_seq = Some(self.newest_seq.map_or(seq, |n| n.max(seq)));

        if is_keyframe && self.segment_ts != Some(frame_ts) {
            self.open_segment(seq, frame_ts);
        }

        // Advancing the frontier may have overwritten the segment head; if so the
        // remaining segment would start mid-frame, so it is no longer replayable.
        if let (Some(start), Some(newest)) = (self.segment_start_seq, self.newest_seq)
            && start.wrapping_add(STREAM_CACHE_CAPACITY as u64) <= newest
        {
            self.segment_ts = None;
            self.segment_start_seq = None;
        }
        None
    }

    /// Anchor the segment at the earliest buffered packet belonging to the
    /// keyframe's frame, so parameter-set packets that arrived just ahead of the
    /// IDR (sharing its RTP timestamp) are kept rather than trimmed away.
    fn open_segment(&mut self, kf_seq: u64, frame_ts: u64) {
        let mut start = kf_seq;
        // Walk back over packets of the same frame. Bounded by the window.
        for _ in 0..STREAM_CACHE_CAPACITY {
            let Some(prev) = start.checked_sub(1) else {
                break;
            };
            match self.slot(prev) {
                Some(p) if p.rtp_ts.numer() == frame_ts => start = prev,
                _ => break,
            }
        }
        self.segment_start_seq = Some(start);
        self.segment_ts = Some(frame_ts);
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
        let start = self.segment_start_seq?;
        let newest = self.newest_seq?;

        // Walk the contiguous run of sequence numbers from the segment start and
        // stop at the first hole. Iterating by sequence number yields an ordered,
        // deduplicated, gap-free segment for free — no sort, dedup, or separate
        // gap-trim pass. Stopping at the gap matters: the burst is emitted with
        // `rewrite_sequential`, which renumbers it onto contiguous output sequence
        // numbers and would erase an internal gap, making a frame whose marker was
        // lost upstream read as complete. The rest arrives on the live cursor,
        // where the gap is preserved.
        let mut segment: Vec<RtpPacket> = Vec::new();
        let mut seq = start;
        while seq <= newest {
            match self.slot(seq) {
                Some(p) => segment.push(p.clone()),
                None => break,
            }
            seq += 1;
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

        // Only a keyframe the H.264 probe could actually read needs a
        // parameter-set prefix. Everything else — VP8/VP9/AV1, or H.264 under
        // SFrame/E2EE — leaves the NAL flags empty, has a self-sufficient
        // keyframe, and offers no parameter sets to synthesize from.
        //
        // Do not key this off the Dependency Descriptor: plain readable H.264
        // negotiates DD too, and skipping its prefix ships an IDR the decoder
        // cannot initialize on, which renders as a blank stream with nothing
        // reporting an error. Do not key it off cached SPS/PPS either — those
        // can arrive after the IDR under reordering.
        let needs_parameter_sets = segment.iter().any(|p| p.nal.idr());

        let mut out = if needs_parameter_sets {
            self.parameter_set_prefix(&segment, segment_ts)?
        } else {
            Vec::new()
        };
        out.extend(segment);

        debug_assert!(
            !needs_parameter_sets || out.iter().any(|p| p.nal.sps()),
            "replay lacks SPS"
        );
        debug_assert!(
            !needs_parameter_sets || out.iter().any(|p| p.nal.pps()),
            "replay lacks PPS"
        );
        debug_assert!(out.iter().any(|p| p.is_keyframe), "replay lacks a keyframe");
        debug_assert!(
            out.windows(2).all(|w| *w[0].seq_no <= *w[1].seq_no),
            "replay must be ordered by sequence number"
        );
        Some(out)
    }

    /// The live read for a subscriber following this stream: every buffered
    /// packet strictly after `cursor`, in sequence order.
    ///
    /// Borrows the ring and yields references, so the hot path allocates nothing
    /// — the reader clones only the packets it emits. In steady state this yields
    /// the single newly-arrived packet: the scan spans `(cursor, newest]`,
    /// clamped to the live window, which is one element when the reader is
    /// current. O(k), no allocation.
    pub fn range_after(&self, cursor: SeqNo) -> impl Iterator<Item = &RtpPacket> + '_ {
        let cursor = *cursor;
        let (lo, hi) = match self.newest_seq {
            Some(newest) if newest > cursor => {
                let floor = newest.saturating_sub(STREAM_CACHE_CAPACITY as u64 - 1);
                (cursor.wrapping_add(1).max(floor), newest)
            }
            // Empty range: nothing newer than the cursor.
            _ => (1, 0),
        };
        (lo..=hi).filter_map(move |seq| self.slot(seq))
    }

    /// The buffered packet with this input sequence number, if still cached.
    ///
    /// Used by the tail drain and reorder backfill to complete a frame from a
    /// known hole: O(1) index + tag check, no scan.
    pub fn get(&self, seq: SeqNo) -> Option<&RtpPacket> {
        self.slot(*seq)
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
        for slot in self.ring.iter_mut() {
            *slot = None;
        }
        self.newest_seq = None;
        self.segment_start_seq = None;
        self.segment_ts = None;
        self.sps = None;
        self.pps = None;
    }
}

/// One track's worth of cache: a `StreamCache` per simulcast encoding, keyed by
/// `rid`. The track is the routing unit; the encoding a packet belongs to is read
/// from `pkt.ext_vals.rid`. A downstream forwarder holds the whole thing so it can
/// replay a target encoding's keyframe segment while still forwarding the current
/// one.
#[derive(Debug, Default)]
pub struct TrackStreamCache {
    encodings: Vec<(Option<Rid>, StreamCache)>,
}

impl TrackStreamCache {
    pub fn new() -> Self {
        Self {
            encodings: Vec::with_capacity(MAX_SIMULCAST_ENCODINGS),
        }
    }

    /// Route `pkt` into its encoding's cache, reading the encoding from the
    /// packet's `rid` (absent for non-simulcast tracks).
    /// Returns the packet only when it was too old to store; otherwise the
    /// cache owns it and the caller reads it back with [`Self::get`].
    pub fn push(&mut self, pkt: RtpPacket) -> Option<RtpPacket> {
        self.encoding_mut(pkt.ext_vals.rid).push(pkt)
    }

    pub fn encoding(&self, rid: Option<Rid>) -> Option<&StreamCache> {
        self.encodings
            .iter()
            .find(|(r, _)| *r == rid)
            .map(|(_, c)| c)
    }

    fn encoding_mut(&mut self, rid: Option<Rid>) -> &mut StreamCache {
        if let Some(pos) = self.encodings.iter().position(|(r, _)| *r == rid) {
            return &mut self.encodings[pos].1;
        }
        debug_assert!(
            self.encodings.len() < MAX_SIMULCAST_ENCODINGS,
            "a track should never carry more than {MAX_SIMULCAST_ENCODINGS} encodings"
        );
        self.encodings.push((rid, StreamCache::default()));
        &mut self.encodings.last_mut().unwrap().1
    }
}

/// Simulcast tops out at three spatial layers; the `+1` leaves room for a
/// non-rid encoding (`None`) coexisting during renegotiation without reallocating.
const MAX_SIMULCAST_ENCODINGS: usize = 4;

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
            cache.push(p.clone());
        }
        let last = b.delta_frame(1);
        for p in &last {
            cache.push(p.clone());
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
            cache.push(p.clone());
        }
        for p in b.delta_frames(3, 2) {
            cache.push(p.clone());
        }
        // A later IDR with no parameter sets of its own.
        let kf = b.keyframe(2);
        for p in &kf {
            cache.push(p.clone());
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
    fn a_dd_keyframe_replays_under_an_opaque_payload_without_parameter_sets() {
        // SFrame/E2EE: the payload is opaque, so no SPS/PPS can ever be probed or
        // synthesized. RtpPacket::default carries an opaque payload with empty NAL
        // flags. The keyframe is known only from the Dependency Descriptor, whose
        // presence makes the segment self-sufficient. Replay must still succeed —
        // otherwise a switch into an encrypted encoding could never land.
        use pulsebeam_core::dd::temporal::TemporalDdGenerator;

        let mut g = TemporalDdGenerator::new(3);
        let mut cache = StreamCache::new();

        let mut kf = RtpPacket::default();
        kf.seq_no = SeqNo::from(1u64);
        kf.rtp_ts = MediaTime::new(1000, kf.rtp_ts.frequency());
        kf.marker = true;
        kf.is_keyframe = true;
        kf.ext_vals
            .user_values
            .set_arc(std::sync::Arc::new(g.next(true)));
        cache.push(kf.clone());

        let mut delta = RtpPacket::default();
        delta.seq_no = SeqNo::from(2u64);
        delta.rtp_ts = MediaTime::new(4000, delta.rtp_ts.frequency());
        delta.marker = true;
        delta
            .ext_vals
            .user_values
            .set_arc(std::sync::Arc::new(g.next(false)));
        cache.push(delta.clone());

        let replay = cache
            .replay()
            .expect("a DD keyframe segment must replay even when the payload is opaque");
        assert!(
            replay.iter().any(|p| p.is_keyframe),
            "replay carries the keyframe"
        );
        assert!(
            replay.iter().all(|p| !p.nal.sps() && !p.nal.pps()),
            "an opaque/E2EE stream gets no synthesized parameter sets"
        );
    }

    #[test]
    fn readable_h264_still_gets_parameter_sets_when_it_also_carries_a_descriptor() {
        // A plain browser H.264 sender negotiates the Dependency Descriptor
        // alongside a readable payload. Its IDR is useless to a decoder without
        // SPS/PPS, so the descriptor must not suppress the prefix — doing so
        // renders as a blank stream with no error anywhere.
        use pulsebeam_core::dd::temporal::TemporalDdGenerator;

        let mut g = TemporalDdGenerator::new(3);
        let mut b = builder(ParameterSetStyle::OnceAtStreamStart);
        let mut cache = StreamCache::new();

        let mut with_dd = |pkts: Vec<RtpPacket>, keyframe: bool| {
            for mut p in pkts {
                p.ext_vals
                    .user_values
                    .set_arc(std::sync::Arc::new(g.next(keyframe && p.is_keyframe)));
                cache.push(p.clone());
            }
        };
        with_dd(b.keyframe(2), true);
        with_dd(b.delta_frames(3, 2), false);
        with_dd(b.keyframe(2), true);

        let replay = cache.replay().expect("replayable");
        assert!(
            replay.iter().any(|p| p.nal.sps()),
            "readable H.264 must carry SPS even when a descriptor is present"
        );
        assert!(
            replay.iter().any(|p| p.nal.pps()),
            "readable H.264 must carry PPS even when a descriptor is present"
        );
    }

    #[test]
    fn a_multi_slice_keyframe_opens_exactly_one_segment() {
        let mut b = builder(ParameterSetStyle::SeparatePacket);
        let mut cache = StreamCache::new();
        let kf = b.keyframe_with_slices(4, 2);
        for p in &kf {
            cache.push(p.clone());
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
            cache.push(p.clone());
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
            cache.push(p.clone());
        }
        assert!(cache.replay().is_some(), "the keyframe alone is free");

        for p in b.delta_frames(2, 1) {
            cache.push(p.clone());
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
        assert!(
            kf.len() > MAX_REPLAY_PACKETS,
            "fixture must exceed the per-frame cap"
        );
        assert!(
            kf.len() <= MAX_REPLAY_PACKETS_HARD,
            "fixture must not exceed the hard cap"
        );
        for p in &kf {
            cache.push(p.clone());
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
            cache.push(p.clone());
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
            cache.push(p.clone());
        }
        assert!(cache.replay().is_some());

        for p in b.delta_frame(4) {
            cache.push(p.clone());
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
            cache.push(p.clone());
        }
        for p in b.delta_frame(1) {
            cache.push(p.clone());
        }
        assert!(
            cache.replay().is_some(),
            "a 5fps screen share must switch from cache, not fall back to a keyframe request"
        );
    }

    #[test]
    fn range_after_returns_only_newer_packets_in_sequence_order() {
        let mut b = builder(ParameterSetStyle::SeparatePacket);
        let mut cache = StreamCache::new();
        for p in b.keyframe(2) {
            cache.push(p.clone());
        }
        let burst = cache.replay().expect("replayable");
        let cursor = burst.last().unwrap().seq_no;

        // Nothing new yet.
        assert_eq!(
            cache.range_after(cursor).count(),
            0,
            "no packets past cursor"
        );

        // A live delta frame arrives.
        let delta = b.delta_frame(2);
        for p in &delta {
            cache.push(p.clone());
        }
        let live: Vec<_> = cache.range_after(cursor).collect();
        assert!(
            live.iter().all(|p| *p.seq_no > *cursor),
            "only packets past the cursor are returned"
        );
        assert!(
            live.windows(2).all(|w| *w[0].seq_no < *w[1].seq_no),
            "packets are returned in sequence order"
        );
        let burst_max = *burst.last().unwrap().seq_no;
        assert!(
            live.iter().all(|p| *p.seq_no > burst_max),
            "packets already in the burst are not returned again"
        );
    }

    #[test]
    fn range_after_and_get_survive_reordered_inserts() {
        let mut b = builder(ParameterSetStyle::SeparatePacket);
        let mut cache = StreamCache::new();
        let kf = b.keyframe(2);
        for p in &kf {
            cache.push(p.clone());
        }
        let cursor = *kf.last().unwrap().seq_no;

        // A delta frame arrives with its packets in reverse order.
        let mut delta = b.delta_frame(3);
        delta.reverse();
        for p in &delta {
            cache.push(p.clone());
        }

        // The ring is keyed by sequence number, so the read is ordered regardless
        // of arrival order, and point lookups find each packet.
        let live: Vec<_> = cache.range_after(cursor.into()).collect();
        assert!(
            live.windows(2).all(|w| *w[0].seq_no < *w[1].seq_no),
            "reordered inserts still read back in sequence order"
        );
        for p in &delta {
            assert_eq!(
                cache.get(p.seq_no).map(|q| q.seq_no),
                Some(p.seq_no),
                "get finds a reordered packet by its sequence number"
            );
        }
    }

    #[test]
    fn a_packet_evicted_from_the_ring_reads_as_absent() {
        let mut b = builder(ParameterSetStyle::SeparatePacket);
        let mut cache = StreamCache::new();
        for p in b.keyframe(2) {
            cache.push(p.clone());
        }
        let old = b.delta_frame(1)[0].clone();
        cache.push(old.clone());
        assert!(cache.get(old.seq_no).is_some(), "present right after push");

        // Push more than a full ring's worth so `old`'s slot is overwritten by a
        // newer sequence number; the tag check must report the old seq as absent.
        for p in b.delta_frames(STREAM_CACHE_CAPACITY + 4, 1) {
            cache.push(p.clone());
        }
        assert!(
            cache.get(old.seq_no).is_none(),
            "an evicted sequence number must not resolve to the newer packet in its slot"
        );
    }

    #[test]
    fn a_stream_without_a_keyframe_is_never_replayable() {
        let mut b = builder(ParameterSetStyle::SeparatePacket);
        let mut cache = StreamCache::new();
        let frames = b.delta_frames(5, 2);
        for p in &frames {
            cache.push(p.clone());
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
            cache.push(p.clone());
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
            cache.push(p.clone());
        }
        assert!(cache.has_keyframe());

        let flood = b.delta_frames(STREAM_CACHE_CAPACITY, 2);
        for p in &flood {
            cache.push(p.clone());
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
            cache.push(p.clone());
        }
        assert!(cache.replay().is_some(), "clean keyframe is replayable");

        // Delta frame (seq 1003-1003, ts=T1 > T0).
        let delta = b.delta_frame(1);
        let delta_ts = delta[0].rtp_ts.numer();
        assert!(delta_ts > kf_ts, "delta must have a higher timestamp");
        for p in &delta {
            cache.push(p.clone());
        }

        // Simulate a late IDR fragment: same ts as keyframe (T0) but
        // seq_no higher than the delta packets (so it sorts last in the burst).
        let delta_last_seq = *delta.last().unwrap().seq_no;
        let mut late_frag = kf.last().unwrap().clone();
        late_frag.seq_no = (delta_last_seq + 1).into();
        // seq = delta_last_seq+1 is still within the keyframe's segment (same ts=T0).
        cache.push(late_frag.clone());

        // The burst is now [kf_seqs..., delta_seqs..., late_frag_seq] sorted by
        // seq_no, with timestamps [T0, ..., T1, ..., T0] — non-monotonic.
        assert!(
            cache.replay().is_none(),
            "a burst with non-monotonic timestamps must be refused to prevent egress violation"
        );
    }
}
