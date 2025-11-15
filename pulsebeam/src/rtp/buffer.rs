use crate::rtp::RtpPacket;
use pulsebeam_runtime::collections::ring::RingBuffer;
use str0m::rtp::SeqNo;

const KEYFRAME_BUFFER_CAPACITY: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// The buffer has not yet seen a keyframe start.
    /// Packets arriving in this state are ignored unless they are a keyframe start.
    Waiting,
    /// A keyframe start has been received, and we are buffering subsequent packets.
    /// If a newer keyframe arrives, the buffer will be reset to that new keyframe.
    /// This state persists until `pop` is called for the first time.
    Buffering,
    /// The consumer has started popping packets. The buffer will now drain
    /// sequentially and skip any gaps from packet loss. It will not reset
    /// if a new keyframe arrives, as it's committed to the current frame.
    Draining,
}

/// A buffer to capture a complete keyframe from a new stream.
///
/// This buffer is designed to robustly handle the initial packet stream from a new
/// media source. Its primary goals are:
/// 1. Ignore all packets until the first keyframe is detected.
/// 2. Guarantee that the first packet returned to the consumer is the keyframe start.
/// 3. Handle newer keyframes that might arrive before the consumer is ready for the old one.
/// 4. Allow the consumer to drain all buffered packets sequentially, skipping gaps from packet loss.
#[derive(Debug)]
pub struct KeyframeBuffer {
    ring: RingBuffer<RtpPacket>,
    state: State,
    /// The sequence number of the keyframe that we are currently buffering or draining.
    keyframe_start_seq: Option<SeqNo>,
    /// The next sequence number we expect to pop. This is used to handle out-of-order
    /// packets and to detect and skip gaps.
    next_pop_seq: Option<SeqNo>,
}

impl Default for KeyframeBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl KeyframeBuffer {
    pub fn new() -> Self {
        Self {
            ring: RingBuffer::new(KEYFRAME_BUFFER_CAPACITY),
            state: State::Waiting,
            keyframe_start_seq: None,
            next_pop_seq: None,
        }
    }

    /// Returns `true` if a keyframe has been received and the buffer is ready to be drained.
    pub fn is_ready(&self) -> bool {
        self.state == State::Buffering || self.state == State::Draining
    }

    /// Adds an RTP packet to the buffer, applying state-specific logic.
    pub fn push(&mut self, pkt: RtpPacket) {
        if pkt.is_keyframe_start {
            // A new keyframe has arrived. We must handle it based on our current state.
            match self.state {
                State::Waiting | State::Buffering => {
                    // If we were waiting for a keyframe, or buffering an old one that
                    // the consumer hasn't started reading yet, we reset to this new keyframe.
                    self.ring.advance_tail_to(*pkt.seq_no); // Evict any older packets.
                    self.keyframe_start_seq = Some(pkt.seq_no);
                    self.next_pop_seq = Some(pkt.seq_no);
                    self.state = State::Buffering;
                }
                State::Draining => {
                    // We are already draining a keyframe. We will buffer this new one,
                    // but we do not change state. The consumer is committed to the current frame.
                }
            }
        } else {
            // This is not a keyframe packet.
            if self.state == State::Waiting {
                // We have not seen a keyframe yet, so this packet is not renderable. Discard it.
                return;
            }
        }

        // For Buffering and Draining states, we always insert the packet.
        self.ring.insert(*pkt.seq_no, pkt);
    }

    /// Pops the next packet in sequence if the buffer is ready.
    ///
    /// This method will skip over any gaps in the sequence numbers caused by packet loss.
    pub fn pop(&mut self) -> Option<RtpPacket> {
        match self.state {
            State::Waiting => {
                // Not ready, no keyframe has been received yet.
                None
            }
            State::Buffering => {
                // This is the first time pop is called for this keyframe.
                // Transition to Draining and proceed.
                self.state = State::Draining;
                self.pop_next_in_sequence()
            }
            State::Draining => self.pop_next_in_sequence(),
        }
    }

    /// Internal helper to find and return the next available packet in sequence.
    fn pop_next_in_sequence(&mut self) -> Option<RtpPacket> {
        let mut next_seq = self.next_pop_seq?;

        // Search for the next available packet, starting from `next_pop_seq`.
        // We limit the search to the buffer's capacity to prevent infinite loops.
        for _ in 0..KEYFRAME_BUFFER_CAPACITY {
            if let Some(pkt) = self.ring.remove(*next_seq) {
                // Found the next packet. Update our expected sequence and return it.
                self.next_pop_seq = Some(next_seq.wrapping_add(1).into());
                return Some(pkt);
            } else {
                // Packet is missing. Check the next sequence number.
                next_seq = next_seq.wrapping_add(1).into();
            }
        }

        // We searched a reasonable range and found no subsequent packets.
        None
    }
}
