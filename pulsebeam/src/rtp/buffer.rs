use crate::rtp::RtpPacket;
use pulsebeam_runtime::collections::ring::RingBuffer;
use str0m::rtp::SeqNo;

const KEYFRAME_BUFFER_CAPACITY: usize = 256;

/// A buffer to capture a complete keyframe from a new stream.
/// It waits for a packet with `is_keyframe_start` and then drains all
/// subsequent packets in-order.
pub struct KeyframeBuffer {
    ring: RingBuffer<RtpPacket>,
    start_seq_no: Option<SeqNo>,
}

impl KeyframeBuffer {
    pub fn new() -> Self {
        Self {
            ring: RingBuffer::new(KEYFRAME_BUFFER_CAPACITY),
            start_seq_no: None,
        }
    }

    /// Returns `true` if the buffer has received the keyframe start and is ready.
    pub fn is_ready(&self) -> bool {
        self.start_seq_no.is_some()
    }

    /// Adds an RTP packet to the buffer.
    pub fn push(&mut self, pkt: RtpPacket) {
        if pkt.is_keyframe_start {
            let new_start_seq = pkt.seq_no;
            if self.start_seq_no.is_some() {
                self.ring.advance_tail_to(*new_start_seq);
            }
            self.start_seq_no = Some(new_start_seq);
        }

        self.ring.insert(*pkt.seq_no, pkt);
    }

    /// Pops the next packet in sequence if the buffer is ready.
    pub fn pop(&mut self) -> Option<RtpPacket> {
        let start_seq = self.start_seq_no?;

        while let Some(pkt) = self.ring.pop_front() {
            if pkt.seq_no < start_seq {
                continue;
            }
            return Some(pkt);
        }

        None
    }
}
