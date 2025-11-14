use crate::rtp::RtpPacket;
use std::{cmp::Reverse, collections::BinaryHeap};
use str0m::rtp::SeqNo;

const KEYFRAME_BUFFER_CAPACITY: usize = 256;

#[derive(Debug, Eq, PartialEq)]
struct OrderedRtpPacket(RtpPacket);

impl PartialOrd for OrderedRtpPacket {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrderedRtpPacket {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.seq_no.cmp(&other.0.seq_no)
    }
}

/// A buffer to capture a complete keyframe from a new stream.
/// It waits for a packet with `is_keyframe_start` and then drains all
/// subsequent packets in-order.
pub struct KeyframeBuffer {
    buffer: BinaryHeap<Reverse<OrderedRtpPacket>>,
    start_seq_no: Option<SeqNo>,
}

impl KeyframeBuffer {
    pub fn new() -> Self {
        Self {
            buffer: BinaryHeap::with_capacity(KEYFRAME_BUFFER_CAPACITY),
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
                self.buffer.retain(|item| item.0.0.seq_no >= new_start_seq);
            }
            self.start_seq_no = Some(new_start_seq);
        }

        // Only add the packet if it's not ancient relative to a keyframe we've already found.
        if let Some(start_seq) = self.start_seq_no
            && pkt.seq_no < start_seq
        {
            return;
        }

        if self.buffer.len() >= KEYFRAME_BUFFER_CAPACITY {
            tracing::warn!("keyframe buffer is full, dropping packets unexpectedly");
            return;
        }

        self.buffer.push(Reverse(OrderedRtpPacket(pkt)));
    }

    /// Pops the next packet in sequence if the buffer is ready.
    pub fn pop(&mut self) -> Option<RtpPacket> {
        let start_seq = self.start_seq_no?;

        while let Some(item) = self.buffer.pop() {
            let pkt = item.0.0;
            if pkt.seq_no < start_seq {
                continue;
            }
            return Some(pkt);
        }

        None
    }
}
