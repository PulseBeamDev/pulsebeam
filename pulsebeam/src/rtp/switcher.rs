use crate::rtp::{Frequency, RtpPacket};
use std::cmp::Ordering;
use std::collections::{BinaryHeap, VecDeque};
use str0m::media::MediaTime;
use str0m::rtp::SeqNo;
use tokio::time::Instant;

/// The maximum amount of time a packet can be held in the buffer past the last
/// emitted packet's playout time. This prevents head-of-line blocking if a
/// keyframe is lost or a stream dies.
const MAX_BUFFER_DURATION: std::time::Duration = std::time::Duration::from_millis(500);

/// An internal anchor point mapping a synchronized server `playout_time`
/// to the continuous, rewritten RTP timestamp of our output stream.
#[derive(Debug, Clone, Copy)]
struct RtpAnchor {
    playout_time: Instant,
    rtp_ts: MediaTime,
}

/// The internal state of the implicit switcher.
#[derive(Debug, PartialEq, Eq)]
enum State {
    /// Waiting for the very first keyframe from any stream.
    Bootstrap,
    /// Actively streaming packets from a single, continuous source.
    Streaming { ssrc: u32 },
}

/// A wrapper for `RtpPacket` to implement a min-heap based on `playout_time`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BufferedPacket(RtpPacket);

impl Ord for BufferedPacket {
    fn cmp(&self, other: &Self) -> Ordering {
        // Reverse ordering to make BinaryHeap a min-heap on playout_time
        other.0.playout_time.cmp(&self.0.playout_time)
    }
}

impl PartialOrd for BufferedPacket {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Manages switching between multiple RTP input streams to produce a single,
/// continuous, and smoothly rewritten output RTP stream.
/// This version uses an "implicit" control model, inferring the desired stream
/// from the packets it receives.
#[derive(Debug)]
pub struct Switcher {
    clock_rate: Frequency,
    state: State,
    buffer: BinaryHeap<BufferedPacket>,
    anchor: Option<RtpAnchor>,
    output_seq_no: SeqNo,
}

impl Switcher {
    /// Creates a new `Switcher` using the provided `clock_rate`.
    /// It starts in a bootstrap state, waiting for any keyframe.
    pub fn new(clock_rate: Frequency) -> Self {
        Self {
            clock_rate,
            state: State::Bootstrap,
            buffer: BinaryHeap::new(),
            anchor: None,
            output_seq_no: SeqNo::from(rand::random::<u16>() as u64),
        }
    }

    /// Pushes a synchronized packet into the switcher's buffer for consideration.
    pub fn push(&mut self, packet: RtpPacket) {
        if packet.playout_time.is_none() {
            return;
        }
        self.buffer.push(BufferedPacket(packet));
    }

    /// Pops the next available packet in the rewritten sequence.
    pub fn pop(&mut self) -> Option<RtpPacket> {
        // Use a temporary drain buffer to allow for rewinding logic on switch.
        let mut temp_drain: VecDeque<BufferedPacket> = VecDeque::new();

        loop {
            // If the main buffer is empty, process anything we drained.
            if self.buffer.is_empty() {
                self.buffer.extend(temp_drain);
                return None;
            }

            // Purge old packets first.
            if let Some(anchor) = self.anchor {
                if let Some(next_packet) = self.buffer.peek() {
                    if anchor.playout_time + MAX_BUFFER_DURATION
                        < next_packet.0.playout_time.unwrap()
                    {
                        self.buffer.pop();
                        continue;
                    }
                }
            }

            let candidate_packet = self.buffer.peek().unwrap();
            let ssrc = *candidate_packet.0.raw_header.ssrc;
            let is_keyframe = candidate_packet.0.is_keyframe_start;

            let should_emit = match self.state {
                State::Bootstrap => is_keyframe,
                State::Streaming { ssrc: current_ssrc } => ssrc == current_ssrc || is_keyframe,
            };

            if should_emit {
                let mut packet = self.buffer.pop().unwrap().0;
                let new_ssrc: u32 = *packet.raw_header.ssrc;

                let is_switching = match self.state {
                    State::Streaming { ssrc: old_ssrc } => new_ssrc != old_ssrc,
                    _ => false,
                };

                // Transition to the new state.
                self.state = State::Streaming { ssrc: new_ssrc };
                self.rewrite_packet(&mut packet);

                if is_switching {
                    // A switch just occurred. The main buffer contains a mix of old and new packets.
                    // 1. Drain everything from the main buffer into our temp buffer.
                    temp_drain.extend(self.buffer.drain());
                    // 2. Filter the temp buffer, keeping only packets from the NEW stream.
                    temp_drain.retain(|p| *p.0.raw_header.ssrc == new_ssrc);
                    // 3. Put the cleaned packets back into the main buffer for the next pop() call.
                    self.buffer.extend(temp_drain);
                }

                return Some(packet);
            } else {
                // The next packet is a delta from a non-active stream. Buffer it temporarily.
                temp_drain.push_back(self.buffer.pop().unwrap());
            }
        }
    }

    /// Returns `true` if the switcher is in a stable `Streaming` state.
    /// This can be used by higher levels to know when it's safe to potentially
    /// trigger a switch by sending a new keyframe.
    pub fn is_stable(&self) -> bool {
        matches!(self.state, State::Streaming { .. })
    }

    /// Rewrites the RTP headers to be continuous and updates the internal state.
    fn rewrite_packet(&mut self, packet: &mut RtpPacket) {
        self.output_seq_no = self.output_seq_no.wrapping_add(1).into();

        let new_rtp_ts = if let Some(anchor) = self.anchor {
            let playout_delta = packet
                .playout_time
                .unwrap()
                .saturating_duration_since(anchor.playout_time);

            let rtp_delta = (playout_delta.as_secs_f64() * self.clock_rate.get() as f64) as u64;
            MediaTime::new(
                anchor.rtp_ts.numer().wrapping_add(rtp_delta),
                self.clock_rate,
            )
        } else {
            MediaTime::new(rand::random::<u32>() as u64, self.clock_rate)
        };

        self.anchor = Some(RtpAnchor {
            playout_time: packet.playout_time.unwrap(),
            rtp_ts: new_rtp_ts,
        });

        packet.seq_no = self.output_seq_no;
        packet.rtp_ts = new_rtp_ts;

        packet.raw_header.sequence_number = *packet.seq_no as u16;
        packet.raw_header.timestamp = packet.rtp_ts.numer() as u32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtp::VIDEO_FREQUENCY;
    use crate::rtp::test_utils::{generate, next_frame};
    use std::time::Duration;
    use str0m::rtp::RtpHeader;

    fn create_synced_packet(
        ssrc: u32,
        seq: u16,
        ts: u32,
        playout_offset_ms: u64,
        is_key: bool,
    ) -> RtpPacket {
        let base_time = Instant::now();
        RtpPacket {
            raw_header: RtpHeader {
                ssrc: ssrc.into(),
                sequence_number: seq,
                timestamp: ts,
                ..Default::default()
            },
            seq_no: (seq as u64).into(),
            rtp_ts: MediaTime::new(ts as u64, VIDEO_FREQUENCY),
            playout_time: Some(base_time + Duration::from_millis(playout_offset_ms)),
            is_keyframe_start: is_key,
            ..Default::default()
        }
    }

    #[test]
    fn test_implicit_start() {
        let ssrc1 = 1111;
        let mut switcher = Switcher::new(VIDEO_FREQUENCY);

        // Push a delta frame, nothing should come out.
        let p1 = create_synced_packet(ssrc1, 100, 1000, 0, false);
        switcher.push(p1);
        assert!(switcher.pop().is_none());
        assert!(!switcher.is_stable());

        // Push a keyframe, now it should start streaming.
        let p2 = create_synced_packet(ssrc1, 101, 4000, 33, true);
        switcher.push(p2);
        let out = switcher.pop().unwrap();
        assert_eq!(*out.raw_header.ssrc, ssrc1);
        assert_eq!(switcher.state, State::Streaming { ssrc: ssrc1 });
        assert!(switcher.is_stable());
    }

    #[test]
    fn test_implicit_switch() {
        let ssrc1 = 1111;
        let ssrc2 = 2222;
        let mut switcher = Switcher::new(VIDEO_FREQUENCY);

        // Start with stream 1
        switcher.push(create_synced_packet(ssrc1, 100, 1000, 0, true));
        let out1 = switcher.pop().unwrap();
        assert_eq!(*out1.raw_header.ssrc, ssrc1);
        assert!(switcher.is_stable());

        // Push a delta from stream 1 (should be forwarded)
        switcher.push(create_synced_packet(ssrc1, 101, 4000, 33, false));
        let out2 = switcher.pop().unwrap();
        assert_eq!(*out2.raw_header.ssrc, ssrc1);

        // Push a delta from stream 2 (should be buffered, nothing emitted)
        switcher.push(create_synced_packet(ssrc2, 500, 50000, 66, false));
        assert!(switcher.pop().is_none());

        // Push a keyframe from stream 2 that is later in time (should trigger switch)
        switcher.push(create_synced_packet(ssrc2, 501, 53000, 99, true));

        // The next packet popped should be the keyframe from ssrc2
        let out3 = switcher.pop().unwrap();
        assert_eq!(*out3.raw_header.ssrc, ssrc2);
        assert_eq!(*out3.seq_no, *out2.seq_no + 1);
        assert_eq!(switcher.state, State::Streaming { ssrc: ssrc2 });

        // The delta frame from stream 2 that arrived before the keyframe should NOT have been purged.
        // It should be the next packet out.
        let out4 = switcher.pop().unwrap();
        assert_eq!(*out4.raw_header.ssrc, ssrc2);
        assert_eq!(*out4.seq_no, *out3.seq_no + 1);
    }
}
