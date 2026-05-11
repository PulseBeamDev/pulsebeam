use crate::rtp::RtpPacket;
use str0m::rtp::SeqNo;
use tokio::time::Instant;

const KEYFRAME_BUFFER_CAPACITY: usize = 128;

#[derive(Debug)]
pub struct KeyframeBuffer {
    ring: Vec<Option<RtpPacket>>,
    head: SeqNo,
    tail: SeqNo,
    /// (seq_no of the first packet in the keyframe group, rtp_ts numerator of that group).
    /// Two packets belong to the same H264 access unit when they share the same rtp_ts,
    /// so we only advance this pointer on a timestamp change (new GOP).
    segment: Option<(SeqNo, u64)>,
    initialized: bool,
}

impl Default for KeyframeBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyframeBuffer {
    pub fn new() -> Self {
        Self {
            ring: vec![None; KEYFRAME_BUFFER_CAPACITY],
            head: 0.into(),
            tail: 0.into(),
            segment: None,
            initialized: false,
        }
    }

    pub fn is_ready(&self, _target_playout: Instant) -> bool {
        self.has_keyframe_segment()
    }

    pub fn has_keyframe_segment(&self) -> bool {
        self.segment.is_some()
    }

    pub fn reset_to(&mut self, seq_no: SeqNo) {
        self.head = seq_no.wrapping_add(1).into();
        self.tail = seq_no;
        self.ring.fill(None);
        self.segment.take();
    }

    pub fn clear(&mut self) {
        self.reset_to(0.into());
        self.initialized = false;
    }

    pub fn is_empty(&self) -> bool {
        self.tail >= self.head
    }

    pub fn push(&mut self, pkt: RtpPacket) {
        if !self.initialized {
            self.reset_to(pkt.seq_no);
            self.initialized = true;
        }

        if *pkt.seq_no + (self.ring.len() as u64) < *self.head {
            tracing::warn!(
                "{} is behind the sliding window, head={}&tail={}, dropping.",
                pkt.seq_no,
                self.head,
                self.tail
            );
            return;
        }

        if pkt.seq_no >= self.head {
            let diff = (*pkt.seq_no - *self.tail) as usize;
            if diff >= 2 * self.ring.len() {
                self.reset_to(pkt.seq_no);
                tracing::debug!("very large jump detected, reset to {}", pkt.seq_no);
            } else if diff >= self.ring.len() {
                let to_drop = (diff - self.ring.len()) + 1;
                // tracing::warn!(
                //     head = *self.head,
                //     tail = *self.tail,
                //     "large jump detected seq_no={}, has to drop {} packets",
                //     pkt.seq_no,
                //     to_drop
                // );
                let new_tail = (*self.tail + to_drop as u64).into();
                self.advance_to(new_tail);
            }
            self.head = (*pkt.seq_no + 1).into();
        }

        if pkt.seq_no < self.tail {
            self.tail = pkt.seq_no;
        }

        if pkt.is_keyframe_start {
            self.segment = match self.segment.take() {
                // Advance only when a keyframe from a *new* GOP arrives (different rtp_ts).
                // H264 SPS, PPS, and IDR all have is_keyframe_start=true but share the same
                // rtp_ts within one access unit; keeping the segment at the SPS ensures they
                // are all forwarded and the decoder receives the parameter sets it needs.
                Some(seg) if pkt.seq_no > seg.0 && pkt.rtp_ts.numer() != seg.1 => {
                    Some((pkt.seq_no, pkt.rtp_ts.numer()))
                }
                None => Some((pkt.seq_no, pkt.rtp_ts.numer())),
                res => res,
            };
        }

        let idx = self.as_index(pkt.seq_no);
        self.ring[idx] = Some(pkt);
    }

    pub fn pop(&mut self) -> Option<RtpPacket> {
        let segment = self.segment?;
        while self.tail < self.head {
            let idx = self.as_index(self.tail);
            let item = &mut self.ring[idx];
            self.tail = (*self.tail + 1).into();
            let Some(pkt) = item.take() else {
                continue;
            };

            // packets before the keyframe must be dropped, they are not renderable
            if pkt.seq_no < segment.0 {
                continue;
            }

            return Some(pkt);
        }

        // Buffer fully drained — clear the segment marker so has_keyframe_segment()
        // returns false and ready_to_stream() correctly reports the buffer as empty.
        self.segment = None;
        None
    }

    fn advance_to(&mut self, new_tail: SeqNo) {
        let to_drop = *new_tail - *self.tail;
        if let Some(segment) = self.segment
            && segment.0 < new_tail
        {
            self.segment = None;
        }

        for _ in 0..to_drop {
            let idx = self.as_index(self.tail);
            self.ring[idx] = None;
            self.tail = (*self.tail + 1).into();
        }
    }

    fn as_index(&self, seq: SeqNo) -> usize {
        (*seq % self.ring.len() as u64) as usize
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::time::Duration;
    use str0m::media::{Frequency, MediaTime};

    fn make(seq: u64, playout: Instant, is_keyframe: bool) -> RtpPacket {
        RtpPacket {
            seq_no: seq.into(),
            playout_time: playout,
            is_keyframe_start: is_keyframe,
            ..Default::default()
        }
    }

    /// Build a packet with an explicit RTP timestamp, for H264 GOP-boundary tests.
    fn make_with_ts(seq: u64, rtp_ts_val: u64, is_keyframe: bool) -> RtpPacket {
        RtpPacket {
            seq_no: seq.into(),
            rtp_ts: MediaTime::new(rtp_ts_val, Frequency::NINETY_KHZ),
            is_keyframe_start: is_keyframe,
            ..Default::default()
        }
    }

    /// H264 often sends SPS, PPS, and IDR as three *separate* RTP packets that
    /// all carry the same RTP timestamp and all have `is_keyframe_start = true`.
    /// The buffer must preserve ALL THREE so the downstream decoder receives the
    /// parameter sets it needs.  Without the fix the segment pointer advances
    /// past SPS and PPS to IDR, causing them to be silently dropped.
    #[test]
    fn test_h264_sps_pps_idr_same_ts_all_preserved() {
        let mut buf = KeyframeBuffer::new();
        let ts = 90_000u64;

        buf.push(make_with_ts(100, ts, true)); // SPS
        buf.push(make_with_ts(101, ts, true)); // PPS
        buf.push(make_with_ts(102, ts, true)); // IDR

        assert_eq!(buf.pop().unwrap().seq_no, 100.into(), "SPS must not be dropped");
        assert_eq!(buf.pop().unwrap().seq_no, 101.into(), "PPS must not be dropped");
        assert_eq!(buf.pop().unwrap().seq_no, 102.into(), "IDR must be returned");
        assert!(buf.pop().is_none());
    }

    /// A new GOP (different RTP timestamp) must advance the segment, dropping
    /// the previous GOP's packets so the decoder always starts at a clean boundary.
    #[test]
    fn test_h264_new_gop_advances_segment() {
        let mut buf = KeyframeBuffer::new();
        let ts_a = 90_000u64;
        let ts_b = 180_000u64;

        buf.push(make_with_ts(100, ts_a, true)); // GOP A: IDR
        buf.push(make_with_ts(101, ts_a, false)); // GOP A: P-frame

        // New GOP arrives (different timestamp).
        buf.push(make_with_ts(102, ts_b, true)); // GOP B: SPS
        buf.push(make_with_ts(103, ts_b, true)); // GOP B: PPS
        buf.push(make_with_ts(104, ts_b, true)); // GOP B: IDR

        // Segment must have advanced to GOP B; GOP A packets dropped.
        assert_eq!(buf.pop().unwrap().seq_no, 102.into(), "GOP B SPS first");
        assert_eq!(buf.pop().unwrap().seq_no, 103.into(), "GOP B PPS second");
        assert_eq!(buf.pop().unwrap().seq_no, 104.into(), "GOP B IDR third");
        assert!(buf.pop().is_none());
    }

    #[test]
    fn test_reordering() {
        let mut buf = KeyframeBuffer::new();
        let now = Instant::now();

        buf.push(make(1, now + Duration::from_millis(10), false));
        buf.push(make(3, now + Duration::from_millis(30), false));
        buf.push(make(2, now + Duration::from_millis(20), false));

        // can't pop yet until we get a keyframe starter
        assert!(buf.pop().is_none());
        buf.push(make(0, now, true));

        assert_eq!(buf.pop().unwrap().seq_no, 0.into());
        assert_eq!(buf.pop().unwrap().seq_no, 1.into());
        assert_eq!(buf.pop().unwrap().seq_no, 2.into());
        assert_eq!(buf.pop().unwrap().seq_no, 3.into());
    }

    #[test]
    fn test_sliding_window_capacity() {
        let mut buf = KeyframeBuffer::new();
        let now = Instant::now();
        let cap = KEYFRAME_BUFFER_CAPACITY as u64;

        buf.push(make(0, now, true));
        buf.push(make(cap - 1, now, false));

        assert_eq!(buf.tail, 0.into());
        buf.push(make(cap, now, false));
        assert_eq!(buf.tail, 1.into());

        let idx = buf.as_index(0.into());
        assert_eq!(buf.ring[idx].as_ref().unwrap().seq_no, cap.into());
    }

    #[test]
    fn test_massive_jump_resets() {
        let mut buf = KeyframeBuffer::new();
        let now = Instant::now();

        buf.push(make(10, now, true));

        // Push packet far in future (>> 2 * capacity)
        let huge_seq = 10 + (KEYFRAME_BUFFER_CAPACITY * 5) as u64;
        buf.push(make(huge_seq, now, true));

        // Buffer should have reset. Tail is now the new sequence.
        assert_eq!(buf.tail, huge_seq.into());
        assert_eq!(buf.head, (huge_seq + 1).into());

        assert_eq!(buf.pop().unwrap().seq_no, huge_seq.into());
    }

    #[test]
    fn test_gaps_are_skipped() {
        let mut buf = KeyframeBuffer::new();
        let now = Instant::now();

        buf.push(make(10, now, true));
        // Skip 11
        buf.push(make(12, now, false));

        assert_eq!(buf.pop().unwrap().seq_no, 10.into());
        // Next pop should skip the None at 11 and find 12
        assert_eq!(buf.pop().unwrap().seq_no, 12.into());
    }

    #[test]
    fn test_is_ready() {
        let mut buf = KeyframeBuffer::new();
        let now = Instant::now();
        let future = now + Duration::from_millis(100);

        // Not ready initially
        assert!(!buf.is_ready(now));

        // Push keyframe
        buf.push(make(1, future, true));

        // is_ready now just checks for presence of a keyframe
        assert!(buf.is_ready(now));
        assert!(buf.is_ready(future + Duration::from_millis(100)));
    }
}
