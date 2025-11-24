use crate::rtp::RtpPacket;
use std::collections::VecDeque;
use str0m::rtp::SeqNo;
use tokio::time::Instant;

const KEYFRAME_BUFFER_CAPACITY: usize = 128;

#[derive(Debug)]
pub struct KeyframeBuffer {
    ring: VecDeque<Option<RtpPacket>>,
    tail: SeqNo,
    segment: Option<(SeqNo, Instant)>,
}

impl Default for KeyframeBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyframeBuffer {
    pub fn new() -> Self {
        Self {
            ring: VecDeque::from(vec![None; KEYFRAME_BUFFER_CAPACITY]),
            tail: 0.into(),
            segment: None,
        }
    }

    pub fn is_ready(&self, target_playout: Instant) -> bool {
        self.segment
            .map(|s| s.1 >= target_playout)
            .unwrap_or_default()
    }

    pub fn reset_to(&mut self, seq_no: SeqNo) {
        self.tail = seq_no;
        self.ring.clear();
    }

    pub fn push(&mut self, pkt: RtpPacket) {
        if pkt.seq_no < self.tail {
            tracing::warn!(
                "{} is behind the current tail {}, dropping.",
                pkt.seq_no,
                self.tail
            );
            return;
        }

        let diff = pkt.seq_no.wrapping_sub(*self.tail) as usize;
        if diff >= 2 * self.ring.capacity() {
            self.reset_to(pkt.seq_no);
        } else {
            while diff >= self.ring.capacity() {
                self.pop();
            }
        }

        if let Some(segment) = self.segment.take() {
            let new_segment = if pkt.seq_no > segment.0 {
                (pkt.seq_no, pkt.playout_time)
            } else {
                segment
            };

            self.segment = Some(new_segment);
        }

        let idx = self.as_index(pkt.seq_no);
        self.ring[idx] = Some(pkt);
    }

    pub fn pop(&mut self) -> Option<RtpPacket> {
        let Some(segment) = self.segment else {
            return None;
        };

        while let Some(item) = self.ring.pop_front() {
            self.tail = self.tail.wrapping_add(1).into();
            let Some(pkt) = item else {
                continue;
            };

            // packets before the keyframe must be dropped, they are not renderable
            if pkt.playout_time < segment.1 {
                continue;
            }

            return Some(pkt);
        }

        None
    }

    fn as_index(&self, seq: SeqNo) -> usize {
        let idx = seq.wrapping_sub(*self.tail);
        idx as usize
    }
}
