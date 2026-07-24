use std::{
    collections::{BTreeMap, VecDeque},
    time::Duration,
};

use crate::rtp::RtpPacket;
use str0m::rtp::SeqNo;
use tokio::time::Instant;

const KEYFRAME_BUFFER_CAPACITY: usize = 128;
const MAX_TRACKED_FRAMES: usize = 8;
const PLAYOUT_JITTER_TOLERANCE: Duration = Duration::from_millis(50);

#[derive(Debug)]
struct FrameState {
    is_keyframe: bool,
    has_marker: bool,
    min_seqno: SeqNo,
    found_boundary: bool,
}

impl Default for FrameState {
    fn default() -> Self {
        Self {
            is_keyframe: false,
            has_marker: false,
            min_seqno: SeqNo::from(u64::MAX),
            found_boundary: false,
        }
    }
}

#[derive(Debug)]
struct RingBuffer {
    ring: Vec<Option<RtpPacket>>,
}

impl RingBuffer {
    fn new() -> Self {
        Self {
            ring: vec![None; KEYFRAME_BUFFER_CAPACITY],
        }
    }

    fn packet_mut(&mut self, seq: SeqNo) -> &mut Option<RtpPacket> {
        let idx = self.index(seq);
        &mut self.ring[idx]
    }

    fn packet(&self, seq: SeqNo) -> &Option<RtpPacket> {
        let idx = self.index(seq);
        &self.ring[idx]
    }

    fn on_frame_boundary(&self, seqno: SeqNo) -> bool {
        let prev: SeqNo = seqno.wrapping_sub(1).into();
        let Some(prev_pkt) = self.packet(prev) else {
            return false;
        };

        prev_pkt.marker && prev_pkt.seq_no == prev
    }

    fn index(&self, seq: SeqNo) -> usize {
        (*seq % self.ring.len() as u64) as usize
    }

    fn clear(&mut self) {
        self.ring.fill(None);
    }

    fn len(&self) -> usize {
        self.ring.len()
    }
}

#[derive(Debug)]
pub struct KeyframeBuffer {
    ring: RingBuffer,
    frames: BTreeMap<Instant, FrameState>,

    pending: VecDeque<RtpPacket>,
    /// The playout time of the most recently flushed segment, once one has
    /// flushed. Gates `append`: a packet that arrives late enough to have
    /// missed the boundary/cold-start detection for its own (earlier)
    /// frame, but happens to straggle in after a *later* frame has already
    /// flushed and started draining, must not be tacked onto the tail of
    /// that later frame's release queue -- it belongs before it, and
    /// appending it out of chronological order corrupts the reassembled
    /// access-unit sequence instead of just losing one stale frame.
    segment_start: Option<Instant>,
    /// True until this buffer's first segment ever flushes (since
    /// construction or the last `clear()`). A brand-new acquisition (a
    /// slot's very first layer, or a fresh switch target) starts with a
    /// genuinely empty ring -- there is no preceding frame to find a
    /// boundary against, and there never will be, since nothing was ever
    /// staged before this target started. That's expected, not a sign of
    /// loss, so the first keyframe seen by a cold buffer is allowed to
    /// start a segment without one. Once warm, a missing boundary again
    /// means real loss/reordering and is correctly treated as unsafe to
    /// splice on.
    cold: bool,
}

impl Default for KeyframeBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyframeBuffer {
    pub fn new() -> Self {
        Self {
            ring: RingBuffer::new(),
            frames: BTreeMap::new(),
            pending: VecDeque::with_capacity(KEYFRAME_BUFFER_CAPACITY),
            segment_start: None,
            cold: true,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    fn segment_within_tolerance(segment_playout: Instant, target_playout: Instant) -> bool {
        let start = target_playout
            .checked_sub(PLAYOUT_JITTER_TOLERANCE)
            .unwrap_or(target_playout);
        // The keyframe can legitimately be ahead of the last emitted packet:
        // the original optimistic transition deliberately starts that new
        // layer as soon as its keyframe boundary is seen.
        start <= segment_playout
    }

    pub fn push(&mut self, pkt: RtpPacket, target_playout: Instant) -> bool {
        let playout_time = pkt.playout_time;
        if !self.frames.contains_key(&playout_time)
            && self.frames.len() == MAX_TRACKED_FRAMES
            && let Some((&oldest, _)) = self.frames.first_key_value()
        {
            self.frames.remove(&oldest);
        }

        let frame = self.frames.entry(playout_time).or_default();
        frame.is_keyframe |= pkt.is_keyframe;
        frame.min_seqno = frame.min_seqno.min(pkt.seq_no);
        frame.has_marker |= pkt.marker;
        frame.found_boundary |= self.ring.on_frame_boundary(frame.min_seqno);

        let seqno = pkt.seq_no;
        *self.ring.packet_mut(seqno) = Some(pkt);

        // Start optimistically when the preceding frame boundary and the first
        // keyframe packet are present. Waiting for this keyframe's final RTP
        // marker is incorrect here: Switcher transitions immediately and the
        // remaining keyframe packets are then forwarded through the new route.
        // A cold buffer has no boundary to find in the first place -- see
        // `cold`'s doc comment -- so it's exempted from that requirement.
        let is_segment = frame.is_keyframe
            && (frame.found_boundary || self.cold)
            && Self::segment_within_tolerance(playout_time, target_playout);
        if is_segment {
            self.flush(playout_time);
            self.cold = false;
        }
        is_segment
    }

    pub fn pop(&mut self) -> Option<RtpPacket> {
        self.pending.pop_front()
    }

    /// Appends a packet directly to the release queue, bypassing boundary
    /// detection. Once a segment has already been identified (the caller's
    /// `Switcher` is committed to switching to it), every further packet
    /// for the same target is unconditionally part of it -- re-running
    /// `push`'s ring/boundary detection on them would fail (the ring was
    /// cleared by the segment's own flush) and strand them forever instead
    /// of forwarding the rest of the keyframe.
    ///
    /// Dropped instead if it's older than the flushed segment's own start:
    /// a genuine straggler from *before* the released frame (delayed enough
    /// to miss its own boundary/cold-start detection, but not delayed
    /// enough to be dropped by the network) must not be tacked onto the
    /// tail of a later frame's release queue -- unlike `flush`'s own ring
    /// scan, `append` has no ordering pass to put it back in place, so
    /// letting it through here would corrupt the reassembled access-unit
    /// sequence instead of just losing one stale, now-unusable frame.
    pub fn append(&mut self, pkt: RtpPacket) {
        if self
            .segment_start
            .is_some_and(|start| pkt.playout_time < start)
        {
            return;
        }
        // Insert in sequence order rather than blindly at the back: under
        // real network jitter, a packet belonging to the segment already
        // draining can arrive after a *later*-sequenced packet of the same
        // frame was already appended (the ring only captures whatever had
        // arrived by flush time; anything still in flight lands here,
        // out of arrival order relative to its neighbors). Appending it
        // blindly would splice it into the wrong position in the
        // reassembled access unit instead of its correct one.
        let idx = self
            .pending
            .iter()
            .position(|queued| queued.seq_no > pkt.seq_no)
            .unwrap_or(self.pending.len());
        self.pending.insert(idx, pkt);
    }

    fn flush(&mut self, start_at: Instant) -> bool {
        let Some(state) = self.frames.get(&start_at) else {
            debug_assert!(false, "segments and frames are out-of-sync");
            return false;
        };

        self.pending.clear();
        let mut current_seq = state.min_seqno;
        for _ in 0..self.ring.len() {
            let slot = self.ring.packet_mut(current_seq);

            if let Some(pkt) = slot.take() {
                // Keep only the segment start frame and newer packets. This avoids
                // leaking packets from older frames that collide in the ring index.
                if pkt.playout_time >= start_at {
                    self.pending.push_back(pkt);
                }
            }

            current_seq = current_seq.wrapping_add(1).into();
        }

        self.ring.clear();
        self.frames.clear();
        self.segment_start = Some(start_at);
        true
    }

    pub fn clear(&mut self) {
        self.pending.clear();
        self.ring.clear();
        self.frames.clear();
        self.segment_start = None;
        self.cold = true;
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::time::Duration;

    fn make(seq: u64, playout: Instant, is_keyframe: bool, marker: bool) -> RtpPacket {
        RtpPacket {
            seq_no: seq.into(),
            playout_time: playout,
            is_keyframe,
            marker,
            ..Default::default()
        }
    }

    #[test]
    fn cold_buffer_segments_its_first_keyframe_without_a_preceding_boundary() {
        let mut buf = KeyframeBuffer::new();
        let now = Instant::now();

        // A brand-new acquisition (a slot's very first layer, or a fresh
        // switch target): the very first packet this buffer ever sees is a
        // keyframe, with nothing preceding it in the ring. There's no
        // history to find a boundary against and there never will be --
        // that's expected, not a sign of loss, so it must still segment.
        let segmented = buf.push(make(100, now, true, false), now);
        assert!(
            segmented,
            "a cold buffer's first keyframe must start a segment without requiring a boundary"
        );
        assert_eq!(buf.pop().unwrap().seq_no, 100.into());
        assert!(buf.pop().is_none());
    }

    #[test]
    fn warm_buffer_still_requires_a_boundary_after_its_first_segment() {
        let mut buf = KeyframeBuffer::new();
        let now = Instant::now();

        // Cold-start the buffer with an initial segment.
        assert!(buf.push(make(100, now, true, false), now));
        assert!(buf.pop().is_some());

        // A later keyframe arrives with nothing preceding it in the ring
        // (e.g. the intervening frame was entirely lost) -- once warm, a
        // missing boundary is real loss again, not a fresh acquisition, and
        // must not be treated as a safe segment start.
        let later = now + Duration::from_millis(200);
        let segmented = buf.push(make(500, later, true, false), later);
        assert!(
            !segmented,
            "a warm buffer must still require a real boundary, not just any keyframe"
        );
        assert!(buf.pop().is_none());
    }

    #[test]
    fn optimistically_segments_at_preceding_marker_before_keyframe_marker() {
        let mut buf = KeyframeBuffer::new();
        let now = Instant::now();

        // This is the real simulcast handoff shape: the old frame ends, then
        // the first packet of the new IDR arrives. The IDR's marker is still
        // in the future. Starting here is required for uninterrupted decode.
        assert!(!buf.push(make(99, now - Duration::from_millis(10), false, true), now,));
        let segmented = buf.push(make(100, now, true, false), now);
        assert!(segmented);

        assert_eq!(buf.pop().unwrap().seq_no, 100.into());
        assert!(buf.pop().is_none());
    }

    #[test]
    fn segments_when_playout_is_ahead_of_target() {
        let mut buf = KeyframeBuffer::new();
        let now = Instant::now();
        let far_future = now + Duration::from_millis(100);

        assert!(!buf.push(
            make(199, far_future - Duration::from_millis(1), false, true),
            now,
        ));
        assert!(buf.push(make(200, far_future, true, false), now));
        assert_eq!(buf.pop().unwrap().seq_no, 200.into());
        assert!(buf.pop().is_none());
    }

    #[test]
    fn segments_when_playout_is_slightly_behind_target() {
        let mut buf = KeyframeBuffer::new();
        let now = Instant::now();
        let behind = now - Duration::from_millis(40);

        assert!(!buf.push(
            make(249, behind - Duration::from_millis(1), false, true),
            now,
        ));
        assert!(buf.push(make(250, behind, true, false), now));

        assert_eq!(buf.pop().unwrap().seq_no, 250.into());
        assert!(buf.pop().is_none());
    }

    #[test]
    fn does_not_segment_when_playout_is_too_old() {
        let mut buf = KeyframeBuffer::new();
        let now = Instant::now();
        let old = now - Duration::from_millis(400);

        assert!(!buf.push(make(299, old - Duration::from_millis(1), false, true), now));
        assert!(!buf.push(make(300, old, true, false), now));
        assert!(buf.pop().is_none());
    }

    #[test]
    fn preserves_out_of_order_packets_within_segment() {
        let mut buf = KeyframeBuffer::new();
        let now = Instant::now();

        // Boundary packet from previous frame.
        assert!(!buf.push(make(9, now - Duration::from_millis(10), false, true), now,));

        // Same frame packets arrive out-of-order.
        assert!(!buf.push(make(11, now, false, false), now));
        assert!(!buf.push(make(12, now, false, true), now));
        assert!(buf.push(make(10, now, true, false), now));

        assert_eq!(buf.pop().unwrap().seq_no, 10.into());
        assert_eq!(buf.pop().unwrap().seq_no, 11.into());
        assert_eq!(buf.pop().unwrap().seq_no, 12.into());
        assert!(buf.pop().is_none());
    }

    #[test]
    fn append_delivers_packets_after_the_ring_was_cleared_by_a_flush() {
        let mut buf = KeyframeBuffer::new();
        let now = Instant::now();

        // Detect and flush the initial segment (clears ring/frames).
        assert!(!buf.push(make(9, now - Duration::from_millis(10), false, true), now));
        assert!(buf.push(make(10, now, true, false), now));
        assert_eq!(buf.pop().unwrap().seq_no, 10.into());
        assert!(buf.pop().is_none());

        // The rest of the keyframe arrives afterward. Re-running `push` on
        // it would never flush again (no preceding marked packet survives
        // the clear), so it must go through `append` instead.
        buf.append(make(11, now, true, false));
        buf.append(make(12, now, true, true));

        assert_eq!(buf.pop().unwrap().seq_no, 11.into());
        assert_eq!(buf.pop().unwrap().seq_no, 12.into());
        assert!(buf.pop().is_none());
    }

    #[test]
    fn append_drops_a_straggler_older_than_the_flushed_segment() {
        let mut buf = KeyframeBuffer::new();
        let now = Instant::now();

        // Cold-start flush of a keyframe at `now`.
        assert!(buf.push(make(100, now, true, false), now));
        assert_eq!(buf.pop().unwrap().seq_no, 100.into());

        // A packet belonging to an *earlier* frame than the one just
        // flushed straggles in late (delayed enough on the network to miss
        // its own boundary/cold-start detection entirely, but not delayed
        // enough to be dropped outright). It must not be spliced onto the
        // tail of the segment that already started draining -- that frame
        // is chronologically obsolete and unusable at this point.
        let straggler_time = now - Duration::from_millis(33);
        buf.append(make(50, straggler_time, false, true));

        // A genuine continuation of the flushed segment (same or later
        // playout time) must still go through normally.
        buf.append(make(101, now, true, true));

        assert_eq!(
            buf.pop().unwrap().seq_no,
            101.into(),
            "a stale straggler from before the flushed segment must be dropped, not \
             delivered ahead of (or interleaved with) the segment that already started"
        );
        assert!(buf.pop().is_none());
    }

    #[test]
    fn append_inserts_a_late_arriving_packet_in_sequence_order() {
        let mut buf = KeyframeBuffer::new();
        let now = Instant::now();

        // Cold-start flush captures only what had arrived by then -- 102's
        // slot in the ring is still empty.
        assert!(buf.push(make(100, now, true, false), now));
        assert_eq!(buf.pop().unwrap().seq_no, 100.into());

        // Same frame's remaining packets arrive out of sequence order (real
        // network jitter can delay any one of them independently) -- 103
        // shows up before 102.
        buf.append(make(103, now, false, false));
        buf.append(make(102, now, false, false));
        buf.append(make(104, now, false, true));

        assert_eq!(buf.pop().unwrap().seq_no, 102.into());
        assert_eq!(buf.pop().unwrap().seq_no, 103.into());
        assert_eq!(
            buf.pop().unwrap().seq_no,
            104.into(),
            "append must insert by sequence number, not arrival order, so a frame's \
             packets reassemble correctly regardless of which one is delayed"
        );
        assert!(buf.pop().is_none());
    }

    #[test]
    fn clear_resets_all_buffered_state() {
        let mut buf = KeyframeBuffer::new();
        let now = Instant::now();

        assert!(!buf.push(make(49, now - Duration::from_millis(1), false, true), now,));
        assert!(buf.push(make(50, now, true, false), now));
        assert!(buf.pop().is_some());

        buf.clear();

        assert!(buf.is_empty());
        assert!(buf.pop().is_none());
    }

    #[test]
    fn frame_state_is_bounded_without_flush() {
        let mut buf = KeyframeBuffer::new();
        let now = Instant::now();

        for i in 0..(MAX_TRACKED_FRAMES as u64 * 8) {
            let playout = now + Duration::from_millis(i);
            let _ = buf.push(make(1000 + i, playout, false, false), now);
        }

        assert!(buf.frames.len() <= MAX_TRACKED_FRAMES);
    }
}
