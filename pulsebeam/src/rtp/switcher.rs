use crate::rtp::{Frequency, RtpPacket};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
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

/// The internal state of the switcher.
#[derive(Debug, PartialEq, Eq)]
enum State {
    /// The switcher is waiting for the first keyframe from the initial stream.
    Bootstrap { ssrc: u32 },
    /// Actively streaming packets from a single, continuous source.
    Streaming { ssrc: u32 },
    /// A switch has been requested. We are forwarding the old stream while waiting
    /// for a keyframe from the new target stream.
    Switching { from_ssrc: u32, to_ssrc: u32 },
}

/// A wrapper for `RtpPacket` to implement a min-heap based on `playout_time`.
/// The `BinaryHeap` is a max-heap by default, so we reverse the ordering.
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
pub struct Switcher {
    // The configured clock rate for this media stream (e.g., 90kHz for video).
    clock_rate: Frequency,
    state: State,
    // A min-heap of packets, ordered by their playout_time.
    buffer: BinaryHeap<BufferedPacket>,
    // The anchor for rewriting RTP timestamps.
    anchor: Option<RtpAnchor>,
    // The continuous, rewritten sequence number for the output stream.
    output_seq_no: SeqNo,
}

impl Switcher {
    /// Creates a new `Switcher` that will start by forwarding the stream with `initial_ssrc`
    /// using the provided `clock_rate` for all timestamp calculations.
    pub fn new(initial_ssrc: u32, clock_rate: Frequency) -> Self {
        Self {
            clock_rate,
            state: State::Bootstrap { ssrc: initial_ssrc },
            buffer: BinaryHeap::new(),
            anchor: None,
            // Start with a random sequence number for security (prevents sequence number guessing).
            output_seq_no: SeqNo::from(rand::random::<u16>() as u64),
        }
    }

    /// Pushes a synchronized packet into the switcher's buffer for consideration.
    pub fn push(&mut self, packet: RtpPacket) {
        // We cannot do anything with a packet that hasn't been synchronized.
        if packet.playout_time.is_none() {
            return;
        }

        self.buffer.push(BufferedPacket(packet));
    }

    /// Pops the next available packet in the rewritten sequence.
    ///
    /// This method should be called repeatedly in a loop to drain ready packets, as
    /// pushing a single new packet might make multiple older packets ready to be emitted.
    pub fn pop(&mut self) -> Option<RtpPacket> {
        loop {
            let next_packet = self.buffer.peek()?;

            // Purge old packets to prevent buffer bloat and head-of-line blocking.
            if let Some(anchor) = self.anchor
                && anchor.playout_time + MAX_BUFFER_DURATION < next_packet.0.playout_time.unwrap()
            {
                self.buffer.pop();
                continue; // Check the next packet
            }

            let candidate = &next_packet.0;
            let ssrc = *candidate.raw_header.ssrc;
            let is_keyframe = candidate.is_keyframe_start;

            let should_emit = match self.state {
                State::Bootstrap { ssrc: target_ssrc } => ssrc == target_ssrc && is_keyframe,
                State::Streaming { ssrc: current_ssrc } => ssrc == current_ssrc,
                State::Switching { from_ssrc, to_ssrc } => {
                    (ssrc == to_ssrc && is_keyframe) || ssrc == from_ssrc
                }
            };

            if should_emit {
                let mut packet = self.buffer.pop().unwrap().0;
                let ssrc: u32 = *packet.raw_header.ssrc;

                match self.state {
                    State::Bootstrap { .. } => self.state = State::Streaming { ssrc },
                    State::Switching { to_ssrc, .. } if ssrc == to_ssrc => {
                        let from_ssrc = self.state_from_ssrc();
                        self.buffer.retain(|p| *p.0.raw_header.ssrc != from_ssrc);
                        self.state = State::Streaming { ssrc };
                    }
                    _ => {}
                }

                self.rewrite_packet(&mut packet);
                return Some(packet);
            } else {
                // The next packet in time cannot be emitted yet; wait for more packets.
                return None;
            }
        }
    }

    /// Requests a switch to the stream identified by `ssrc`.
    ///
    /// The switch will only be executed when a keyframe from the new stream is received.
    /// This method will do nothing if a switch to the same SSRC is already pending
    /// or active.
    pub fn switch_to(&mut self, ssrc: u32) {
        self.state = match self.state {
            State::Bootstrap { ssrc: current_ssrc } if current_ssrc != ssrc => {
                State::Bootstrap { ssrc }
            }
            State::Streaming { ssrc: current_ssrc } if current_ssrc != ssrc => State::Switching {
                from_ssrc: current_ssrc,
                to_ssrc: ssrc,
            },
            // If we are already switching, update the target.
            State::Switching { from_ssrc, .. } if from_ssrc != ssrc => State::Switching {
                from_ssrc,
                to_ssrc: ssrc,
            },
            // No change needed in other cases.
            _ => return,
        };
    }

    /// Returns `true` if the switcher is in a stable `Streaming` state and can
    /// accept a new `switch_to` command.
    ///
    /// A higher level should wait for this to be `true` before requesting another switch
    /// to avoid flapping.
    pub fn safe_to_switch(&self) -> bool {
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

    fn state_from_ssrc(&self) -> u32 {
        if let State::Switching { from_ssrc, .. } = self.state {
            from_ssrc
        } else {
            0
        }
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
    fn test_basic_stream_forwarding_with_push_pop() {
        let ssrc1 = 1111;
        let mut switcher = Switcher::new(ssrc1, VIDEO_FREQUENCY);

        let packets = generate(
            create_synced_packet(ssrc1, 100, 1000, 0, true),
            vec![next_frame(), next_frame(), next_frame()],
        );

        let mut output = vec![];
        for p in packets {
            switcher.push(p);
            // In a simple forwarding case, pop will yield a packet as soon as it's pushed
            // (after the initial keyframe).
            while let Some(out) = switcher.pop() {
                output.push(out);
            }
        }

        assert_eq!(output.len(), 4);
        assert!(switcher.safe_to_switch());
        for i in 1..output.len() {
            assert_eq!(*output[i].seq_no, *output[i - 1].seq_no + 1);
        }
    }

    #[test]
    fn test_simple_switch_with_push_pop() {
        let ssrc1 = 1111;
        let ssrc2 = 2222;
        let mut switcher = Switcher::new(ssrc1, VIDEO_FREQUENCY);

        let p1 = create_synced_packet(ssrc1, 100, 1000, 0, true);
        switcher.push(p1);
        let out1 = switcher.pop().unwrap();
        assert!(switcher.safe_to_switch());
        assert_eq!(*out1.raw_header.ssrc, ssrc1);

        switcher.switch_to(ssrc2);
        assert!(!switcher.safe_to_switch());

        let p2 = create_synced_packet(ssrc1, 101, 4000, 33, false);
        switcher.push(p2);
        let out2 = switcher.pop().unwrap();
        assert_eq!(*out2.raw_header.ssrc, ssrc1);

        // At this point, pop returns None because it's waiting for a keyframe from ssrc2
        assert!(switcher.pop().is_none());

        let p3 = create_synced_packet(ssrc2, 500, 50000, 66, true);
        switcher.push(p3);
        let out3 = switcher.pop().unwrap(); // Now the switch can complete
        assert_eq!(*out3.raw_header.ssrc, ssrc2);
        assert!(switcher.safe_to_switch());

        assert_eq!(*out2.seq_no + 1, *out3.seq_no);
        let playout_delta_ms = out3
            .playout_time
            .unwrap()
            .duration_since(out2.playout_time.unwrap())
            .as_millis();
        let rtp_ts_delta = out3.rtp_ts.numer().wrapping_sub(out2.rtp_ts.numer());
        let expected_ts_delta = playout_delta_ms as u64 * (VIDEO_FREQUENCY.get() / 1000) as u64;
        assert!((rtp_ts_delta as i64 - expected_ts_delta as i64).abs() < 90);
    }
}
