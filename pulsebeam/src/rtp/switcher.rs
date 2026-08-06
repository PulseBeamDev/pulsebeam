use pulsebeam_runtime::rand::RngCore;
use std::collections::BTreeSet;
use std::time::Duration;
use str0m::media::{Frequency, MediaTime};
use str0m::rtp::SeqNo;
use tokio::time::Instant;

use crate::entity::TrackId;
use crate::rtp::RtpPacket;
use crate::rtp::cache::{StreamCache, TrackStreamCache};
use crate::rtp::frame_selector::{
    DecodeTargetSelection, DependencyDescriptorSelector, FrameDecision, FrameSelector,
};
use crate::rtp::timeline::Timeline;
use crate::track::StreamId;

/// How long after a switch the abandoned stream may still complete frames the
/// subscriber has already been given part of.
const TAIL_DRAIN_WINDOW: Duration = Duration::from_millis(200);

/// Cap on outstanding holes tracked at once, so a long run of loss cannot grow
/// the set without bound.
const MAX_TRACKED_HOLES: usize = 256;

/// How long a ready switch waits for the active layer to finish the frame it is
/// midway through.
///
/// A frame's packets arrive back-to-back, so this only ever costs a millisecond
/// or two in the normal case; it is short because a packet that has not turned
/// up by now is late enough that waiting is worse than switching. Whatever the
/// switch leaves behind can still be filled afterwards by the drain tail.
const FRAME_ALIGN_GRACE: Duration = Duration::from_millis(15);

/// Translation for the stream a slot has just switched away from.
///
/// Its packets keep arriving for a little while — reordered, or simply still in
/// flight — and some of them belong to frames the subscriber has already been
/// given part of. Holding the old mapping lets those frames be completed instead
/// of leaving the subscriber a fragment it will either render broken or count as
/// congestion loss.
#[derive(Debug)]
struct Tail {
    seq_base: SeqNo,
    ts_base: u64,
    expires_at: Instant,
    /// The gaps this stream left behind. Scoped to the tail so a later stream
    /// can never be translated into a gap belonging to an earlier one.
    holes: BTreeSet<u64>,
}

/// The stream-switching state machine for one downstream video slot.
///
/// The switcher owns which upstream stream a slot forwards and every step of
/// moving between streams. It is driven by two inputs:
///
///   * [`switch_to`](Self::switch_to) — policy: "forward this stream." The
///     switcher stages the target and switches to it at the next clean frame
///     boundary, replaying from its keyframe.
///   * [`feed`](Self::feed) — a stream's [`StreamCache`] has new packets. The
///     switcher reads whatever it needs through a cursor and emits rewritten
///     packets that look to the subscriber like one continuous sender.
///
/// The cache is the single source of packet data: both the initial replay burst
/// and the ongoing live tail are read through the same cursor, so there is no
/// separate live path that could desync from the burst and hand the subscriber
/// a half-frame that looks whole.
#[derive(Debug)]
pub struct Switcher {
    timeline: Timeline,

    /// Intra-encoding frame selection: within the active encoding, which frames to
    /// forward. At its default `Full` target it forwards everything (identical to
    /// the pre-DD forwarder); lowered to a decode target it sheds temporal/spatial
    /// layers frame by frame for fine-grained bitrate control.
    selector: DependencyDescriptorSelector,

    /// The stream currently forwarded, and how far its cache has been read.
    /// `active_cursor` is `Some` whenever `active` is `Some`.
    active: Option<StreamId>,
    active_cursor: Option<SeqNo>,
    /// Input sequence numbers the active stream has skipped over — gaps the live
    /// forward pass has stepped past. Tracked in the stream's own input space and
    /// reset on every switch, so a reordered packet that fills one is looked up in
    /// the cache unambiguously (an output-space hole could alias a stale packet
    /// left in the cache by an earlier stream on the same layer).
    active_input_holes: BTreeSet<u64>,
    /// Next input sequence number the active stream is expected to produce.
    next_expected_input: Option<u64>,
    /// The stream a switch is pending to. Its keyframe is awaited.
    staging: Option<StreamId>,
    /// The stream just switched away from, still completing in-flight frames.
    draining: Option<StreamId>,

    /// When the staged stream first became switchable but the active stream was
    /// still midway through a frame.
    switch_blocked_since: Option<Instant>,

    tail: Option<Tail>,

    /// Highest output sequence number emitted, and the one after it.
    last_output: Option<SeqNo>,
    next_expected_output: Option<u64>,
    /// Output sequence numbers skipped and not yet filled. A packet may only be
    /// emitted into one of these after a switch, which is what makes completing
    /// the old stream's frames incapable of colliding with the new one.
    holes: BTreeSet<u64>,
    /// Output sequence number of the most recent packet that closed a frame, and
    /// the first packet of the newest frame.
    last_marker_output: Option<u64>,
    frame_start_output: Option<u64>,
    frame_ts: Option<u64>,
    /// Maximum output_ts ever emitted on this stream. Used to drop a reordered
    /// live packet that would advance the sequence frontier while going backwards
    /// in time — the exact condition the egress stream invariant forbids.
    max_output_ts: Option<u64>,
}

impl Switcher {
    pub fn new<R: RngCore>(clock_rate: Frequency, rng: &mut R) -> Self {
        Self {
            timeline: Timeline::new(clock_rate, rng),
            selector: DependencyDescriptorSelector::new(),
            active: None,
            active_cursor: None,
            active_input_holes: BTreeSet::new(),
            next_expected_input: None,
            staging: None,
            draining: None,
            switch_blocked_since: None,
            tail: None,
            last_output: None,
            next_expected_output: None,
            holes: BTreeSet::new(),
            last_marker_output: None,
            frame_start_output: None,
            frame_ts: None,
            max_output_ts: None,
        }
    }

    /// Request that the slot forward `target` as soon as it can switch cleanly.
    ///
    /// Idempotent. If already active on `target`, any pending switch away from it
    /// is cancelled. If a switch to a different stream is in flight, it retargets.
    pub fn switch_to(&mut self, target: StreamId) {
        if self.active == Some(target) {
            // We are already on it; drop any pending switch away.
            if self.staging.take().is_some() {
                self.switch_blocked_since = None;
            }
            return;
        }
        if self.staging == Some(target) {
            return;
        }
        // Switching back to the stream currently draining: stop draining it. It
        // will re-enter as active from its own keyframe, and its old tail — a
        // generation now two switches back — is no longer drainable.
        if self.draining == Some(target) {
            self.draining = None;
            self.tail = None;
        }
        self.staging = Some(target);
        self.switch_blocked_since = None;
    }

    /// Stop forwarding and reset the input/role state — which stream this slot
    /// forwards and any switch in flight.
    ///
    /// The output-stream state (the timeline, the last emitted sequence number
    /// and marker, frame bounds, `max_output_ts`, holes) is deliberately kept:
    /// the egress stream is one continuous stream across a pause/resume, so a
    /// burst that resumes after a frame was left open still knows to break the
    /// sequence rather than continue it and read as a whole frame.
    pub fn stop(&mut self) {
        self.active = None;
        self.active_cursor = None;
        self.active_input_holes.clear();
        self.next_expected_input = None;
        self.staging = None;
        self.draining = None;
        self.switch_blocked_since = None;
        self.tail = None;
    }

    /// The track this slot forwards has new packets. Reconciles every role
    /// against the track's cache — pulling the active encoding's live tail,
    /// draining the outgoing encoding's stragglers, and landing a pending switch
    /// as soon as the target encoding is replayable — and emits rewritten packets
    /// via `emit`.
    ///
    /// The whole `TrackStreamCache` is handed in, so a pending switch is
    /// re-evaluated on *any* packet of the track, not only the target encoding's:
    /// the switch lands the moment its keyframe is decodable, without waiting for
    /// the next staging packet to drive it.
    ///
    /// Each role is guarded on `stream.0 == track_id` so a slot briefly
    /// subscribed to two tracks (a track change in flight) never translates one
    /// track's encoding through another track's cache.
    pub fn feed(
        &mut self,
        track_id: TrackId,
        cache: &TrackStreamCache,
        now: Instant,
        emit: &mut impl FnMut(RtpPacket),
    ) {
        if let Some(active) = self.active.filter(|s| s.0 == track_id)
            && let Some(encoding) = cache.encoding(active.1)
        {
            self.pull_active(encoding, emit);
        }
        if let Some(draining) = self.draining.filter(|s| s.0 == track_id)
            && let Some(encoding) = cache.encoding(draining.1)
        {
            self.drain_tail(encoding, now, emit);
        }
        if let Some(staging) = self.staging.filter(|s| s.0 == track_id)
            && let Some(encoding) = cache.encoding(staging.1)
        {
            self.try_switch(encoding, now, emit);
        }
    }

    pub fn active_stream(&self) -> Option<StreamId> {
        self.active
    }

    pub fn staging_stream(&self) -> Option<StreamId> {
        self.staging
    }

    pub fn draining_stream(&self) -> Option<StreamId> {
        self.draining
    }

    /// A switch is pending; the slot should keep sending PLIs for the staged
    /// stream's keyframe.
    pub fn awaiting_switch(&self) -> bool {
        self.staging.is_some()
    }

    /// Set which decode target the active encoding is forwarded at. `Full`
    /// forwards every frame; a lowered target sheds temporal/spatial layers for
    /// finer bitrate control than dropping a whole simulcast encoding.
    pub fn set_decode_target(&mut self, target: DecodeTargetSelection) {
        self.selector.set_target(target);
    }

    pub fn decode_target(&self) -> DecodeTargetSelection {
        self.selector.target()
    }

    /// Pull new live packets from the active stream's cache and emit them,
    /// preserving the stream's own sequence structure so upstream loss stays
    /// visible to the subscriber as a gap.
    ///
    /// Hot path: `range_after` yields the single just-arrived packet in the
    /// common case (O(1)), and the O(holes) backfill runs only on the rare
    /// reorder event where the forward pass produced nothing.
    fn pull_active(&mut self, cache: &StreamCache, emit: &mut impl FnMut(RtpPacket)) {
        let mut forwarded_any = false;

        if let Some(cursor) = self.active_cursor {
            for pkt in cache.range_after(cursor) {
                let input_seq = *pkt.seq_no;
                forwarded_any = true;

                // Drop a packet that would both advance the output sequence
                // frontier AND carry a timestamp behind the frontier — a delayed
                // fragment of an earlier frame with a high sequence number.
                // Forwarding it would trip the egress stream invariant. It is
                // consumed (cursor advances past it) but never emitted, and not
                // recorded as a gap, so the backfill will not resurrect it.
                let output_seq = input_seq.wrapping_add(*self.timeline.seq_base());
                let output_ts = pkt.rtp_ts.numer().wrapping_add(self.timeline.ts_base());
                let advances_frontier = self.last_output.is_none_or(|last| output_seq > *last);
                let drop_backward =
                    advances_frontier && self.max_output_ts.is_some_and(|m| output_ts < m);

                if !drop_backward {
                    // Record the input sequence numbers stepped over as gaps the
                    // stream may still fill by reordering (bounded).
                    if let Some(expected) = self.next_expected_input
                        && input_seq > expected
                        && input_seq - expected <= MAX_TRACKED_HOLES as u64
                    {
                        self.active_input_holes.extend(expected..input_seq);
                        self.trim_active_input_holes();
                    }

                    match self.selector.decide(pkt) {
                        FrameDecision::Drop => {
                            // Intentional layer shed: renumber around it so the
                            // subscriber sees a contiguous stream, not loss.
                            self.timeline.drop_input();
                        }
                        FrameDecision::Forward => {
                            let mut out = pkt.clone();
                            self.timeline.rewrite(&mut out);
                            self.note_emitted(out.seq_no, out.marker, out.rtp_ts.numer());
                            emit(out);
                        }
                    }
                }

                if self.next_expected_input.is_none_or(|e| input_seq >= e) {
                    self.next_expected_input = Some(input_seq.wrapping_add(1));
                }
                self.active_cursor = Some(input_seq.into());
            }
        }

        // Backfill: a packet reordered behind the cursor cannot come through the
        // forward pass (its sequence number is below it), so the forward pass
        // yields nothing this call. If such a packet fills a gap the stream
        // stepped over, emit it so the subscriber sees a whole frame instead of
        // counting the gap as loss. The egress guard tolerates this — a hole-fill
        // never advances the frontier — and the subscriber orders by sequence
        // number regardless of arrival order.
        if !forwarded_any {
            self.backfill_active_holes(cache, emit);
        }
    }

    fn trim_active_input_holes(&mut self) {
        while self.active_input_holes.len() > MAX_TRACKED_HOLES {
            let lowest = *self.active_input_holes.iter().next().expect("non-empty");
            self.active_input_holes.remove(&lowest);
        }
    }

    /// Emit cached packets that fill a gap the active stream stepped over.
    ///
    /// Gaps are tracked in the stream's own input space and reset on every
    /// switch, so the cache lookup is unambiguous — it can only return this
    /// stream's packet, never a stale one an earlier stream left on the layer.
    fn backfill_active_holes(&mut self, cache: &StreamCache, emit: &mut impl FnMut(RtpPacket)) {
        if self.active_input_holes.is_empty() {
            return;
        }
        let holes: Vec<u64> = self.active_input_holes.iter().copied().collect();
        for input_seq in holes {
            let Some(pkt) = cache.get(input_seq.into()) else {
                continue;
            };
            let mut out = pkt.clone();
            self.timeline.rewrite(&mut out);
            self.active_input_holes.remove(&input_seq);
            self.note_emitted(out.seq_no, out.marker, out.rtp_ts.numer());
            emit(out);
        }
    }

    /// Attempt the pending switch to the staged stream.
    ///
    /// Waits for a clean frame boundary on the active stream, then replays the
    /// staged stream's keyframe segment onto a fresh, contiguous output sequence
    /// and promotes it to active. The old active stream begins draining.
    fn try_switch(&mut self, cache: &StreamCache, now: Instant, emit: &mut impl FnMut(RtpPacket)) {
        if !self.may_switch_now(now) {
            return;
        }
        let Some(packets) = cache.replay() else {
            // Not decodable from here yet; the slot's PLI retry keeps probing.
            return;
        };
        let Some(new_cursor) = packets.last().map(|p| p.seq_no) else {
            return;
        };

        // If the previously emitted output frame was left open — whether the old
        // stream is now being drained or the slot was merely paused since — burn
        // one output sequence number so the burst does not continue that frame
        // contiguously, which the subscriber would read as a completed frame.
        // Runs regardless of whether there is an old stream to drain, and before
        // the rebase below so the reserved gap sits ahead of the burst.
        let reserved_hole = if self.newest_frame_left_open()
            && let Some(last) = self.last_output
        {
            self.timeline.skip_output_sequence(1);
            let reserved = (*last).wrapping_add(1);
            self.holes.insert(reserved);
            Some(reserved)
        } else {
            None
        };

        // Hand the outgoing stream a window to complete frames the subscriber has
        // already seen part of. Must run before the rebase below, while the
        // timeline still holds the old stream's translation.
        if self.active.is_some() {
            self.open_tail(now, reserved_hole);
        }

        // Promote: the staged stream becomes active; the old active drains.
        self.draining = self.active;
        self.active = self.staging.take();
        self.active_cursor = Some(new_cursor);
        self.active_input_holes.clear();
        // The new encoding has its own decode-target structure; forward it whole
        // until the allocator chooses a target for it.
        self.selector.set_target(DecodeTargetSelection::Full);
        // The next live packet the stream owes is the one after the burst.
        self.next_expected_input = Some((*new_cursor).wrapping_add(1));
        self.switch_blocked_since = None;

        // Emit the burst on a fresh, rebased, contiguous output sequence.
        let mut first = true;
        for mut pkt in packets {
            if first {
                self.timeline.rebase(&pkt);
                first = false;
            }
            self.timeline.rewrite_sequential(&mut pkt);
            self.note_emitted(pkt.seq_no, pkt.marker, pkt.rtp_ts.numer());
            emit(pkt);
        }

        // Realign so the stream's next live packet (new_cursor + 1) continues
        // straight on from where the burst stopped.
        self.timeline.resync_to_input(new_cursor);
    }

    /// Snapshot the outgoing stream's translation so its in-flight packets can
    /// still complete frames the subscriber has part of.
    ///
    /// Tracks only holes left by the current active stream, plus any gap reserved
    /// for a frame left open during this switch.
    fn open_tail(&mut self, now: Instant, reserved_hole: Option<u64>) {
        debug_assert!(self.active.is_some());
        let seq_base = self.timeline.seq_base();
        let mut holes: BTreeSet<u64> = self
            .active_input_holes
            .iter()
            .map(|input_seq| input_seq.wrapping_add(*seq_base))
            .collect();
        if let Some(reserved) = reserved_hole {
            holes.insert(reserved);
        }
        self.tail = Some(Tail {
            seq_base,
            ts_base: self.timeline.ts_base(),
            expires_at: now + TAIL_DRAIN_WINDOW,
            holes,
        });
    }

    /// Complete frames on the draining stream: fill the holes the switch left
    /// with the matching packets now in the old stream's cache, translated by the
    /// old stream's mapping.
    ///
    /// A straggler that fills a hole has a sequence number *below* the point the
    /// slot had reached on that stream, so it is looked up by the hole it fills
    /// rather than pulled from a cursor — a cursor only ever moves forward.
    fn drain_tail(&mut self, cache: &StreamCache, now: Instant, emit: &mut impl FnMut(RtpPacket)) {
        let Some(tail) = self.tail.as_ref() else {
            self.draining = None;
            return;
        };
        if now >= tail.expires_at {
            self.tail = None;
            self.draining = None;
            return;
        }

        let seq_base = tail.seq_base;
        let ts_base = tail.ts_base;
        let holes: Vec<u64> = tail.holes.iter().copied().collect();
        for output_seq in holes {
            let input_seq: SeqNo = output_seq.wrapping_sub(*seq_base).into();
            let Some(pkt) = cache.get(input_seq) else {
                continue;
            };
            let mut out = pkt.clone();
            out.seq_no = output_seq.into();
            out.rtp_ts = MediaTime::new(
                pkt.rtp_ts.numer().wrapping_add(ts_base),
                pkt.rtp_ts.frequency(),
            );
            emit(out);
            if let Some(tail) = self.tail.as_mut() {
                tail.holes.remove(&output_seq);
            }
            self.holes.remove(&output_seq);
        }

        // Nothing left to complete: retire the tail.
        if self.tail.as_ref().is_some_and(|t| t.holes.is_empty()) {
            self.tail = None;
            self.draining = None;
        }
    }

    pub fn has_tail(&self) -> bool {
        self.tail.is_some()
    }

    /// Whether the switch may land now, or should wait for the active stream to
    /// finish the frame it is partway through.
    fn may_switch_now(&mut self, now: Instant) -> bool {
        if self.at_clean_frame_boundary() {
            self.switch_blocked_since = None;
            return true;
        }
        let blocked_since = *self.switch_blocked_since.get_or_insert(now);
        now.saturating_duration_since(blocked_since) >= FRAME_ALIGN_GRACE
    }

    /// Whether the output currently sits on a completed frame with no holes
    /// left inside it.
    ///
    /// Only the newest frame matters. Holes further back belong to frames the
    /// subscriber has already resolved one way or the other, and waiting on
    /// those — they are usually real upstream loss and will never be filled —
    /// would stall every switch for the full grace period.
    fn at_clean_frame_boundary(&self) -> bool {
        if self.newest_frame_left_open() {
            return false;
        }
        let frame_start = self.frame_start_output.unwrap_or(0);
        self.holes.range(frame_start..).next().is_none()
    }

    /// Whether the highest sequence number emitted so far did not close its
    /// frame, so the next stream would continue straight on from a fragment.
    ///
    /// This asks about the newest packet in sequence order, not the most
    /// recently written one: a reordered packet filling an earlier gap does not
    /// reopen a frame that has already been closed.
    fn newest_frame_left_open(&self) -> bool {
        self.last_output
            .is_some_and(|last| self.last_marker_output != Some(*last))
    }

    fn note_emitted(&mut self, seq: SeqNo, marker: bool, ts: u64) {
        let seq_v = *seq;
        match self.next_expected_output {
            None => {}
            Some(expected) if seq_v == expected => {}
            Some(expected) if seq_v > expected => {
                // A jump this large is a switch, not loss; do not record it.
                if seq_v - expected <= MAX_TRACKED_HOLES as u64 {
                    self.holes.extend(expected..seq_v);
                }
            }
            Some(_) => {
                self.holes.remove(&seq_v);
            }
        }
        while self.holes.len() > MAX_TRACKED_HOLES {
            let lowest = *self.holes.iter().next().expect("non-empty");
            self.holes.remove(&lowest);
        }

        if self.frame_ts != Some(ts) {
            self.frame_ts = Some(ts);
            self.frame_start_output = Some(seq_v);
        }
        if self.next_expected_output.is_none_or(|e| seq_v >= e) {
            self.next_expected_output = Some(seq_v + 1);
        }
        if self.last_output.is_none_or(|last| seq_v > *last) {
            self.last_output = Some(seq);
        }
        if marker {
            self.last_marker_output = Some(seq_v);
        }
        if self.max_output_ts.is_none_or(|m| ts > m) {
            self.max_output_ts = Some(ts);
        }
    }
}

#[cfg(test)]
impl Switcher {
    /// Force the stream roles for test setup, bypassing the normal switch flow.
    pub fn test_set_roles(&mut self, active: Option<StreamId>, staging: Option<StreamId>) {
        self.active = active;
        self.active_cursor = active.map(|_| SeqNo::from(0u64));
        self.staging = staging;
        self.draining = None;
        self.switch_blocked_since = None;
    }

    /// Simulate a burst landing: the staged stream becomes active.
    pub fn test_promote(&mut self) {
        if let Some(staged) = self.staging.take() {
            self.draining = self.active;
            self.active = Some(staged);
            self.active_cursor = Some(SeqNo::from(0u64));
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::entity::{ParticipantId, TrackKind};
    use crate::rtp;
    use crate::rtp::test_utils::{H264StreamBuilder, ParameterSetStyle};
    use pulsebeam_runtime::rand::seeded_rng;
    use str0m::media::Rid;
    use tokio::time::Instant;

    /// Two distinct streams (two simulcast layers of one track) to switch between.
    fn two_streams() -> (StreamId, StreamId) {
        let track = ParticipantId::new(&mut seeded_rng(1)).derive_track_id(TrackKind::Video, "v");
        ((track, Some(Rid::from("q"))), (track, Some(Rid::from("h"))))
    }

    fn builder(ssrc: u32) -> H264StreamBuilder {
        H264StreamBuilder::new(ssrc, 1000, 90_000, Instant::now())
            .with_parameter_sets(ParameterSetStyle::SeparatePacket)
    }

    /// Feed every packet into the track cache then the switcher, mirroring
    /// `route_video`: the packet is stamped with its encoding's rid (as ingress
    /// does) so the track cache routes it to the right per-encoding ring.
    fn ingest(
        switcher: &mut Switcher,
        stream: StreamId,
        cache: &mut TrackStreamCache,
        packets: &[RtpPacket],
        out: &mut Vec<RtpPacket>,
    ) {
        for p in packets {
            let mut p = p.clone();
            p.ext_vals.rid = stream.1;
            cache.push(&p);
            let now = p.arrival_ts;
            switcher.feed(stream.0, cache, now, &mut |o| out.push(o));
        }
    }

    #[test]
    fn initial_subscribe_replays_keyframe_then_follows_live_contiguously() {
        let (q, _) = two_streams();
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(7));
        let mut cache = TrackStreamCache::new();
        let mut b = builder(1);
        let mut out = Vec::new();

        switcher.switch_to(q);

        // Keyframe arrives; the switch replays it as the initial burst.
        let kf = b.keyframe(3);
        ingest(&mut switcher, q, &mut cache, &kf, &mut out);
        assert!(!out.is_empty(), "keyframe burst must be emitted");
        assert_eq!(switcher.active_stream(), Some(q), "slot is now active on q");
        assert!(!switcher.awaiting_switch());
        assert!(
            out.windows(2).all(|w| *w[1].seq_no == *w[0].seq_no + 1),
            "the burst is contiguous"
        );

        // Live delta frames follow, continuing the output sequence seamlessly.
        let burst_last = *out.last().unwrap().seq_no;
        let delta = b.delta_frame(2);
        ingest(&mut switcher, q, &mut cache, &delta, &mut out);
        let live_first = *out[out.len() - delta.len()].seq_no;
        assert_eq!(live_first, burst_last + 1, "live continues from the burst");
    }

    #[test]
    fn live_upstream_loss_stays_visible_as_a_gap() {
        let (q, _) = two_streams();
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(8));
        let mut cache = TrackStreamCache::new();
        let mut b = builder(1);
        let mut out = Vec::new();

        switcher.switch_to(q);
        ingest(&mut switcher, q, &mut cache, &b.keyframe(2), &mut out);
        let before = *out.last().unwrap().seq_no;

        // Four packets lost upstream, then a delta frame.
        b.drop_packets(4);
        let delta = b.delta_frame(1);
        ingest(&mut switcher, q, &mut cache, &delta, &mut out);

        assert_eq!(
            *out.last().unwrap().seq_no - before,
            5,
            "4 lost upstream packets must leave a detectable hole"
        );
    }

    #[test]
    fn a_burst_packet_is_never_emitted_twice_under_reordering() {
        let (q, _) = two_streams();
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(12));
        let mut cache = TrackStreamCache::new();
        let mut b = builder(1);
        let mut out = Vec::new();

        switcher.switch_to(q);
        let kf = b.keyframe(3);
        ingest(&mut switcher, q, &mut cache, &kf, &mut out);
        let burst_len = out.len();

        // The same keyframe packets are redelivered (reordering); none re-emit.
        ingest(&mut switcher, q, &mut cache, &kf, &mut out);
        assert_eq!(
            out.len(),
            burst_len,
            "cursor prevents re-emitting the burst"
        );
    }

    #[test]
    fn switching_layers_stays_contiguous_and_decodable() {
        let (q, h) = two_streams();
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(20));
        let mut cache = TrackStreamCache::new();
        let mut bq = builder(1);
        let mut bh = builder(2);
        let mut out = Vec::new();

        // Establish q.
        switcher.switch_to(q);
        ingest(&mut switcher, q, &mut cache, &bq.keyframe(3), &mut out);
        ingest(&mut switcher, q, &mut cache, &bq.delta_frame(2), &mut out);

        // Request a switch to h; h's keyframe arrives and the switch lands.
        switcher.switch_to(h);
        assert!(switcher.awaiting_switch());
        ingest(&mut switcher, h, &mut cache, &bh.keyframe(3), &mut out);
        assert_eq!(switcher.active_stream(), Some(h));

        // Live h follows.
        ingest(&mut switcher, h, &mut cache, &bh.delta_frame(2), &mut out);

        assert!(
            out.windows(2).all(|w| *w[1].seq_no > *w[0].seq_no),
            "output sequence never goes backwards across the switch"
        );
        assert!(
            out.windows(2)
                .all(|w| w[1].rtp_ts.numer() >= w[0].rtp_ts.numer()),
            "output timestamp never goes backwards across the switch"
        );
    }

    #[test]
    fn a_full_switch_scenario_stays_egress_decodable() {
        // Regression for TruncatedFrameLooksComplete and the backwards-timestamp
        // class: the whole output of a subscribe + soak + layer switch + soak must
        // satisfy the egress invariants, including that no frame is cut short while
        // the next follows on a contiguous sequence number.
        let (q, h) = two_streams();
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(30));
        let mut cache = TrackStreamCache::new();
        let mut bq = builder(1);
        let mut bh = builder(2);
        let mut out = Vec::new();

        switcher.switch_to(q);
        ingest(&mut switcher, q, &mut cache, &bq.keyframe(3), &mut out);
        for _ in 0..10 {
            ingest(&mut switcher, q, &mut cache, &bq.delta_frame(3), &mut out);
        }

        switcher.switch_to(h);
        ingest(&mut switcher, h, &mut cache, &bh.keyframe(3), &mut out);
        for _ in 0..10 {
            ingest(&mut switcher, h, &mut cache, &bh.delta_frame(3), &mut out);
        }

        rtp::conformance::assert_decodable(&out, "subscribe + simulcast switch + soak");
    }

    /// Stamp a parsed Dependency Descriptor on every packet of `frame`, marking it
    /// present or absent for decode-target 0 (a temporal-layer shed).
    fn stamp_dd(frame: &mut [RtpPacket], in_dt0: bool) {
        use pulsebeam_core::dd::{
            DecodeTargetIndication, DependencyDescriptor, FrameDependencyTemplate,
        };
        let dti = if in_dt0 {
            DecodeTargetIndication::Required
        } else {
            DecodeTargetIndication::NotPresent
        };
        for p in frame.iter_mut() {
            let mut dd = DependencyDescriptor::default();
            dd.frame_dependencies = FrameDependencyTemplate {
                dtis: [dti].into_iter().collect(),
                temporal_id: if in_dt0 { 0 } else { 1 },
                ..Default::default()
            };
            p.ext_vals.user_values.set_arc(std::sync::Arc::new(dd));
        }
    }

    #[test]
    fn dd_target_sheds_temporal_frames_and_keeps_egress_contiguous() {
        use crate::rtp::frame_selector::DecodeTargetSelection;

        let (q, _) = two_streams();
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(41));
        let mut cache = TrackStreamCache::new();
        let mut b = builder(1);
        let mut out = Vec::new();

        switcher.switch_to(q);
        ingest(&mut switcher, q, &mut cache, &b.keyframe(2), &mut out);
        let after_keyframe = out.len();

        // Forward only the base temporal layer.
        switcher.set_decode_target(DecodeTargetSelection::Target(0));

        // Alternate base (kept) and enhancement (shed) delta frames.
        let mut kept = 0;
        let mut shed = 0;
        for i in 0..8 {
            let in_dt0 = i % 2 == 0;
            let mut frame = b.delta_frame(1);
            stamp_dd(&mut frame, in_dt0);
            ingest(&mut switcher, q, &mut cache, &frame, &mut out);
            if in_dt0 {
                kept += 1;
            } else {
                shed += 1;
            }
        }

        let live = &out[after_keyframe..];
        assert_eq!(
            live.len(),
            kept,
            "exactly the base-layer frames are forwarded ({kept} kept, {shed} shed)"
        );
        assert!(
            out.windows(2).all(|w| *w[1].seq_no == *w[0].seq_no + 1),
            "egress stays contiguous across shed frames — no gap the subscriber reads as loss"
        );
        rtp::conformance::assert_decodable(&out, "subscribe + temporal shed to dt0");
    }

    #[test]
    fn full_target_forwards_every_frame_even_with_dd() {
        use crate::rtp::frame_selector::DecodeTargetSelection;

        let (q, _) = two_streams();
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(42));
        let mut cache = TrackStreamCache::new();
        let mut b = builder(1);
        let mut out = Vec::new();

        switcher.switch_to(q);
        ingest(&mut switcher, q, &mut cache, &b.keyframe(2), &mut out);
        assert_eq!(switcher.decode_target(), DecodeTargetSelection::Full);
        let after_keyframe = out.len();

        for i in 0..6 {
            let mut frame = b.delta_frame(1);
            stamp_dd(&mut frame, i % 2 == 0);
            ingest(&mut switcher, q, &mut cache, &frame, &mut out);
        }
        assert_eq!(
            out.len() - after_keyframe,
            6,
            "at Full target no frame is shed, DD present or not"
        );
    }

    /// Stamp a real Dependency Descriptor (with frame number and dependencies)
    /// from a temporal generator onto every packet of `frame`.
    fn stamp_generated_dd(frame: &mut [RtpPacket], dd: &pulsebeam_core::dd::DependencyDescriptor) {
        for p in frame.iter_mut() {
            p.ext_vals
                .user_values
                .set_arc(std::sync::Arc::new(dd.clone()));
        }
    }

    /// Assert the forwarded stream is decodable *at the Dependency Descriptor
    /// level*: every forwarded frame's declared references (`frame_diffs`) point
    /// only to frames that were also forwarded. A keyframe (it carries the
    /// structure) is an entry point with no references. This proves the SFU shed a
    /// self-consistent set — not merely that egress RTP is well-formed.
    fn assert_dd_decodable(forwarded: &[RtpPacket]) {
        use pulsebeam_core::dd::DependencyDescriptor;
        use std::collections::HashSet;

        let present: HashSet<u16> = forwarded
            .iter()
            .filter_map(|p| p.ext_vals.user_values.get::<DependencyDescriptor>())
            .map(|dd| dd.frame_number)
            .collect();

        for p in forwarded {
            let Some(dd) = p.ext_vals.user_values.get::<DependencyDescriptor>() else {
                continue;
            };
            if dd.attached_structure.is_some() {
                continue; // keyframe: an independently decodable entry point
            }
            for diff in &dd.frame_dependencies.frame_diffs {
                let referenced = dd.frame_number.wrapping_sub(*diff);
                assert!(
                    present.contains(&referenced),
                    "forwarded frame {} references frame {} which was shed — not decodable",
                    dd.frame_number,
                    referenced
                );
            }
        }
    }

    /// Shedding a temporal target must leave a stream that is decodable by its own
    /// dependency structure, not just one whose RTP is well-formed. Uses the real
    /// `TemporalDdGenerator` so frames carry genuine `frame_diffs`.
    #[test]
    fn dd_shedding_to_a_lower_target_stays_dd_decodable() {
        use crate::rtp::frame_selector::DecodeTargetSelection;
        use pulsebeam_core::dd::temporal::TemporalDdGenerator;

        for target in [
            DecodeTargetSelection::Target(0),
            DecodeTargetSelection::Target(1),
        ] {
            let (q, _) = two_streams();
            let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(51));
            let mut cache = TrackStreamCache::new();
            let mut b = builder(1);
            let mut generator = TemporalDdGenerator::new(3);
            let mut out = Vec::new();

            switcher.switch_to(q);
            let mut kf = b.keyframe(2);
            stamp_generated_dd(&mut kf, &generator.next(true));
            ingest(&mut switcher, q, &mut cache, &kf, &mut out);

            switcher.set_decode_target(target);
            for _ in 0..24 {
                let mut frame = b.delta_frame(1);
                stamp_generated_dd(&mut frame, &generator.next(false));
                ingest(&mut switcher, q, &mut cache, &frame, &mut out);
            }

            assert!(
                out.windows(2).all(|w| *w[1].seq_no == *w[0].seq_no + 1),
                "egress stays contiguous at {target:?}"
            );
            rtp::conformance::assert_decodable(&out, "temporal shed");
            assert_dd_decodable(&out);
        }
    }
}
