use crate::rtp::{Frequency, RtpPacket};
use std::collections::{BTreeMap, VecDeque};
use str0m::media::MediaTime;
use str0m::rtp::{SeqNo, Ssrc};

/// The internal anchor point for rewriting timestamps.
#[derive(Debug, Clone, Copy)]
struct RtpAnchor {
    seq_no: SeqNo,
    rtp_ts: MediaTime,
}

/// Represents a partially or fully assembled frame.
#[derive(Debug, Default)]
struct FrameBuffer {
    packets: BTreeMap<SeqNo, RtpPacket>,
    is_keyframe: bool,
    has_marker: bool,
}

impl FrameBuffer {
    fn is_complete(&self) -> bool {
        self.has_marker
    }

    fn is_complete_keyframe(&self) -> bool {
        self.is_keyframe && self.has_marker
    }

    fn push(&mut self, packet: RtpPacket) {
        if packet.is_keyframe_start {
            self.is_keyframe = true;
        }
        if packet.raw_header.marker {
            self.has_marker = true;
        }
        self.packets.insert(packet.seq_no, packet);
    }
}

/// The internal state of the switcher. It is Copy-able for safe state transitions.
#[derive(Debug, PartialEq, Eq, Copy, Clone)]
enum State {
    /// Waiting for the first complete keyframe from any stream.
    Bootstrap,
    /// Actively streaming from a stable SSRC.
    Streaming { active_ssrc: Ssrc },
    /// Forwarding the old stream while waiting for a keyframe from the new one.
    Switching { old_ssrc: Ssrc, new_ssrc: Ssrc },
}

/// Manages switching between multiple RTP input streams to produce a single,
/// continuous, and smoothly rewritten output RTP stream.
#[derive(Debug)]
pub struct Switcher {
    clock_rate: Frequency,
    state: State,
    /// Buffers frames, grouped by SSRC and then by RTP timestamp.
    buffer: BTreeMap<(Ssrc, MediaTime), FrameBuffer>,
    /// Holds the rewritten packets of the frame currently being emitted.
    output_queue: VecDeque<RtpPacket>,
    /// The timeline for rewriting headers to be continuous.
    anchor: Option<RtpAnchor>,
}

impl Switcher {
    pub fn new(clock_rate: Frequency) -> Self {
        Self {
            clock_rate,
            state: State::Bootstrap,
            buffer: BTreeMap::new(),
            output_queue: VecDeque::new(),
            anchor: None,
        }
    }

    /// Returns `true` if the switcher is in a stable `Streaming` state.
    pub fn is_stable(&self) -> bool {
        matches!(&self.state, State::Streaming { .. })
    }

    /// Pushes a packet into the switcher for frame assembly.
    pub fn push(&mut self, packet: RtpPacket) {
        let ssrc = packet.raw_header.ssrc;
        let ts = packet.rtp_ts.rebase(self.clock_rate);

        // State transition logic on push.
        self.state = match self.state {
            State::Streaming { active_ssrc } if ssrc != active_ssrc => State::Switching {
                old_ssrc: active_ssrc,
                new_ssrc: ssrc,
            },
            State::Switching { old_ssrc, new_ssrc } if ssrc != old_ssrc && ssrc != new_ssrc => {
                // A third SSRC appeared. Prioritize the newest one.
                State::Switching {
                    old_ssrc,
                    new_ssrc: ssrc,
                }
            }
            _ => self.state, // No state change needed.
        };

        self.buffer.entry((ssrc, ts)).or_default().push(packet);
    }

    /// Pops the next available packet in the rewritten sequence.
    pub fn pop(&mut self) -> Option<RtpPacket> {
        if let Some(packet) = self.output_queue.pop_front() {
            return Some(packet);
        }

        self.select_next_frame();
        self.output_queue.pop_front()
    }

    /// Finds the next valid complete frame, rewrites it, and fills the output queue.
    fn select_next_frame(&mut self) {
        let winner_key = match self.state {
            State::Bootstrap => self
                .buffer
                .iter()
                .find(|(_, frame)| frame.is_complete_keyframe())
                .map(|(key, _)| *key),

            State::Streaming { active_ssrc } => self
                .buffer
                .iter()
                .filter(|((ssrc, _), frame)| *ssrc == active_ssrc && frame.is_complete())
                .map(|(key, _)| *key)
                .next(),

            State::Switching { old_ssrc, new_ssrc } => {
                let keyframe = self
                    .buffer
                    .iter()
                    .find(|((ssrc, _), frame)| *ssrc == new_ssrc && frame.is_complete_keyframe());

                if keyframe.is_some() {
                    keyframe.map(|(key, _)| *key)
                } else {
                    self.buffer
                        .iter()
                        .filter(|((ssrc, _), frame)| *ssrc == old_ssrc && frame.is_complete())
                        .map(|(key, _)| *key)
                        .next()
                }
            }
        };

        if let Some(key) = winner_key {
            if let Some(mut frame) = self.buffer.remove(&key) {
                let new_ssrc = key.0;

                self.buffer.retain(|(_, ts), _| *ts > key.1);

                if let State::Switching { old_ssrc, .. } = self.state {
                    if new_ssrc != old_ssrc {
                        self.buffer.retain(|(ssrc, _), _| *ssrc != old_ssrc);
                    }
                }

                self.state = State::Streaming {
                    active_ssrc: new_ssrc,
                };

                if frame.is_keyframe {
                    let first_packet = frame.packets.values().next().unwrap();
                    self.rebase(first_packet);
                }

                while let Some((_, mut packet)) = frame.packets.pop_first() {
                    self.rewrite_packet(&mut packet);
                    self.output_queue.push_back(packet);
                }
            }
        }
    }

    fn rebase(&mut self, packet: &RtpPacket) {
        let new_anchor = if let Some(anchor) = self.anchor {
            RtpAnchor {
                seq_no: anchor.seq_no.wrapping_add(1).into(),
                rtp_ts: anchor.rtp_ts, // This will be updated by the first packet's rewrite.
            }
        } else {
            RtpAnchor {
                seq_no: (rand::random::<u16>() as u64).into(),
                rtp_ts: MediaTime::new(rand::random::<u32>() as u64, self.clock_rate),
            }
        };

        let seq_offset = new_anchor.seq_no.wrapping_sub(*packet.seq_no);
        let ts_offset = new_anchor
            .rtp_ts
            .numer()
            .wrapping_sub(packet.rtp_ts.numer());

        let mut temp_anchor = new_anchor;
        temp_anchor.seq_no = seq_offset.into();
        temp_anchor.rtp_ts = MediaTime::new(ts_offset, self.clock_rate);

        // This simplified rebase just resets the anchor point.
        self.anchor = Some(new_anchor);
    }

    fn rewrite_packet(&mut self, packet: &mut RtpPacket) {
        let anchor = self.anchor.get_or_insert_with(|| RtpAnchor {
            seq_no: packet.seq_no,
            rtp_ts: packet.rtp_ts,
        });

        let next_seq = anchor.seq_no.wrapping_add(1).into();

        packet.seq_no = next_seq;

        self.anchor = Some(RtpAnchor {
            seq_no: next_seq,
            rtp_ts: packet.rtp_ts,
        });

        packet.raw_header.sequence_number = *packet.seq_no as u16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rtp::{RtpPacket, VIDEO_FREQUENCY};
    use std::time::Instant;
    use str0m::rtp::RtpHeader;

    fn create_packet(ssrc: u32, seq: u16, ts: u32, is_key: bool, marker: bool) -> RtpPacket {
        RtpPacket {
            raw_header: RtpHeader {
                ssrc: ssrc.into(),
                sequence_number: seq,
                timestamp: ts,
                marker,
                ..Default::default()
            },
            seq_no: (seq as u64).into(),
            rtp_ts: MediaTime::new(ts as u64, VIDEO_FREQUENCY),
            playout_time: Some(Instant::now()),
            is_keyframe_start: is_key,
            ..Default::default()
        }
    }

    #[test]
    fn test_starts_on_complete_keyframe() {
        let mut switcher = Switcher::new(VIDEO_FREQUENCY);
        switcher.push(create_packet(1111, 100, 1000, true, false));
        assert!(switcher.pop().is_none());
        switcher.push(create_packet(1111, 101, 1000, false, true));

        let out1 = switcher.pop().unwrap();
        assert_eq!(*out1.raw_header.ssrc, 1111);
        assert!(out1.is_keyframe_start);

        let out2 = switcher.pop().unwrap();
        assert_eq!(*out2.seq_no, *out1.seq_no + 1);
        assert!(out2.raw_header.marker);
    }

    #[test]
    fn test_switches_to_new_ssrc_on_keyframe() {
        let mut switcher = Switcher::new(VIDEO_FREQUENCY);
        switcher.push(create_packet(1111, 100, 1000, true, true));
        assert!(switcher.pop().is_some());
        assert!(matches!(switcher.state, State::Streaming { .. }));

        switcher.push(create_packet(2222, 200, 2000, false, false));
        assert!(matches!(switcher.state, State::Switching { .. }));
        assert!(switcher.pop().is_none());

        switcher.push(create_packet(1111, 101, 1500, false, true));
        assert!(
            switcher
                .buffer
                .contains_key(&(1111.into(), MediaTime::new(1500, VIDEO_FREQUENCY)))
        );

        switcher.push(create_packet(2222, 201, 2000, true, true));

        let out1 = switcher.pop().unwrap();
        assert_eq!(*out1.raw_header.ssrc, 2222);
        assert!(out1.is_keyframe_start);

        assert!(
            !switcher
                .buffer
                .contains_key(&(1111.into(), MediaTime::new(1500, VIDEO_FREQUENCY)))
        );
    }
}
