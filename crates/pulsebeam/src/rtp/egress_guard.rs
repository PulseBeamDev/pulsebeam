//! Debug-build guard on the RTP stream the SFU hands to a subscriber.
//!
//! Subscribers have no jitter buffer in front of the SFU, so a forwarding bug
//! reaches the decoder directly and shows up only as visual corruption. These
//! checks turn that into a loud failure under `debug_assertions` and in
//! simulation, where the switching paths are actually exercised.

use ahash::{HashMap, HashMapExt, HashSet};
use std::collections::VecDeque;
use str0m::media::{MediaKind, Mid, Rid};

/// How many timestamps to remember per egress stream when looking for reuse.
const HISTORY: usize = 1024;

#[derive(Debug)]
pub enum EgressViolation {
    /// One output sequence number carried two different packets. Whichever the
    /// subscriber keeps, it loses the other.
    SequenceCollision { seq: u64, ts: u64, previous_ts: u64 },
    /// The stream advanced in sequence but went backwards in RTP time. The
    /// subscriber's decoder cannot order those two frames.
    TimestampWentBackwards { ts: u64, previous: u64 },
    /// The stream advanced in sequence onto a timestamp an earlier frame had
    /// already used, so two distinct frames now claim the same instant.
    ReusedTimestamp { ts: u64 },
    /// A frame ended without its marker bit and the next frame follows with no
    /// sequence gap, so the subscriber reassembles a fragment believing it whole
    /// and hands it to the decoder.
    TruncatedFrameLooksComplete { ts: u64, next_ts: u64 },
}

impl std::fmt::Display for EgressViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SequenceCollision {
                seq,
                ts,
                previous_ts,
            } => write!(
                f,
                "output sequence {seq} carried two different packets \
                 (rtp timestamps {previous_ts} then {ts})"
            ),
            Self::TimestampWentBackwards { ts, previous } => write!(
                f,
                "rtp timestamp went backwards from {previous} to {ts} while the sequence advanced"
            ),
            Self::ReusedTimestamp { ts } => {
                write!(f, "rtp timestamp {ts} reused by a later frame")
            }
            Self::TruncatedFrameLooksComplete { ts, next_ts } => write!(
                f,
                "frame {ts} was cut short but {next_ts} follows contiguously, so the \
                 subscriber cannot tell it is incomplete"
            ),
        }
    }
}

#[derive(Debug, Default)]
struct StreamHistory {
    max_seq: Option<u64>,
    /// Output sequence number to the timestamp it carried, so a packet the
    /// network merely duplicated can be told apart from two distinct packets
    /// landing on one sequence number.
    seqs: HashMap<u64, u64>,
    seq_order: VecDeque<u64>,
    timestamps: HashSet<u64>,
    ts_order: VecDeque<u64>,
    /// Timestamp of the newest frame the stream has reached.
    frontier_ts: Option<u64>,
    /// Sequence number of the newest packet of the frame at `frontier_ts`, and
    /// whether that packet closed the frame.
    frontier_seq: Option<u64>,
    frontier_closed: bool,
}

impl StreamHistory {
    fn check(
        &mut self,
        seq: u64,
        ts: u64,
        marker: bool,
        kind: MediaKind,
    ) -> Option<EgressViolation> {
        let mut violation = None;

        match self.seqs.insert(seq, ts) {
            Some(previous_ts) if previous_ts != ts => {
                violation = Some(EgressViolation::SequenceCollision {
                    seq,
                    ts,
                    previous_ts,
                });
            }
            // A duplicate the network made of a packet already forwarded.
            Some(_) => return violation,
            None => {
                self.seq_order.push_back(seq);
                if self.seq_order.len() > HISTORY
                    && let Some(old) = self.seq_order.pop_front()
                {
                    self.seqs.remove(&old);
                }
            }
        }

        // A packet behind the sequence frontier is one the network reordered or
        // that arrived late; forwarding it with its own older timestamp is
        // correct. Only a packet that advances the stream can move the clock, so
        // only those are held to timestamp monotonicity.
        let advances = self.max_seq.is_none_or(|max| seq > max);
        self.max_seq = Some(self.max_seq.map_or(seq, |max| max.max(seq)));
        if !advances {
            return violation;
        }

        // Crossing into a new frame: whatever the previous one ended on had
        // better have closed it, or left a hole saying it did not.
        //
        // Video only. An Opus packet is a whole frame on its own, and its marker
        // bit means start-of-talkspurt (RFC 3551), not end-of-frame — so an
        // audio packet without one says nothing about completeness.
        if kind == MediaKind::Video
            && let (Some(frontier), Some(frontier_seq)) = (self.frontier_ts, self.frontier_seq)
            && ts > frontier
            && !self.frontier_closed
            && seq == frontier_seq.wrapping_add(1)
        {
            violation.get_or_insert(EgressViolation::TruncatedFrameLooksComplete {
                ts: frontier,
                next_ts: ts,
            });
        }

        if let Some(frontier) = self.frontier_ts {
            if ts < frontier {
                violation.get_or_insert(EgressViolation::TimestampWentBackwards {
                    ts,
                    previous: frontier,
                });
            } else if ts > frontier && !self.timestamps.insert(ts) {
                violation.get_or_insert(EgressViolation::ReusedTimestamp { ts });
            }
        } else {
            self.timestamps.insert(ts);
        }

        if self.frontier_ts.is_none_or(|frontier| ts > frontier) {
            self.ts_order.push_back(ts);
            if self.ts_order.len() > HISTORY
                && let Some(old) = self.ts_order.pop_front()
            {
                self.timestamps.remove(&old);
            }
            self.frontier_ts = Some(ts);
            self.frontier_closed = marker;
        } else if self.frontier_ts == Some(ts) {
            self.frontier_closed |= marker;
        }
        self.frontier_seq = Some(seq);

        violation
    }
}

/// Tracks recent egress history per outbound stream.
#[derive(Debug)]
pub struct EgressGuard {
    streams: HashMap<(Mid, Option<Rid>), StreamHistory>,
}

impl Default for EgressGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl EgressGuard {
    pub fn new() -> Self {
        Self {
            streams: HashMap::new(),
        }
    }

    pub fn check(
        &mut self,
        mid: Mid,
        rid: Option<Rid>,
        seq: u64,
        ts: u64,
        marker: bool,
        kind: MediaKind,
    ) -> Option<EgressViolation> {
        self.streams
            .entry((mid, rid))
            .or_default()
            .check(seq, ts, marker, kind)
    }
}

#[cfg(test)]
mod test {
    // A fixture that overflows should fail the test, not clamp into a pass.
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See crates/pulsebeam/docs/thread-per-core.md.
    use super::*;

    fn mid() -> Mid {
        Mid::from("v0")
    }

    #[test]
    fn a_clean_stream_reports_nothing() {
        let mut guard = EgressGuard::new();
        let mut seq = 100u64;
        for frame in 0..50u64 {
            let ts = 90_000 + frame * 3000;
            for _ in 0..3 {
                assert!(
                    guard
                        .check(mid(), None, seq, ts, true, MediaKind::Video)
                        .is_none()
                );
                seq += 1;
            }
        }
    }

    #[test]
    fn upstream_loss_gaps_are_not_violations() {
        let mut guard = EgressGuard::new();
        assert!(
            guard
                .check(mid(), None, 1, 1000, true, MediaKind::Video)
                .is_none()
        );
        assert!(
            guard
                .check(mid(), None, 9, 4000, true, MediaKind::Video)
                .is_none()
        );
    }

    #[test]
    fn a_late_packet_carrying_an_older_timestamp_is_not_a_violation() {
        let mut guard = EgressGuard::new();
        // Sequence 5 is missing at first; every other packet flows normally.
        for frame in 0..20u64 {
            let seq = frame + 1;
            if seq == 5 {
                continue;
            }
            assert!(
                guard
                    .check(
                        mid(),
                        None,
                        seq,
                        1000 + frame * 3000,
                        true,
                        MediaKind::Video
                    )
                    .is_none()
            );
        }
        // It finally arrives, 15 frames late, still carrying its own timestamp.
        assert!(
            guard
                .check(mid(), None, 5, 1000 + 4 * 3000, true, MediaKind::Video)
                .is_none(),
            "forwarding a late packet with its own timestamp is correct"
        );
    }

    #[test]
    fn a_sequence_number_carrying_two_different_packets_is_reported() {
        let mut guard = EgressGuard::new();
        assert!(
            guard
                .check(mid(), None, 7, 1000, true, MediaKind::Video)
                .is_none()
        );
        assert!(matches!(
            guard.check(mid(), None, 7, 4000, true, MediaKind::Video),
            Some(EgressViolation::SequenceCollision { seq: 7, .. })
        ));
    }

    #[test]
    fn a_packet_the_network_duplicated_is_not_a_violation() {
        let mut guard = EgressGuard::new();
        assert!(
            guard
                .check(mid(), None, 7, 1000, true, MediaKind::Video)
                .is_none()
        );
        assert!(
            guard
                .check(mid(), None, 8, 4000, true, MediaKind::Video)
                .is_none()
        );
        assert!(
            guard
                .check(mid(), None, 7, 1000, true, MediaKind::Video)
                .is_none(),
            "the same packet arriving twice is the network's doing, not ours"
        );
    }

    #[test]
    fn advancing_the_stream_backwards_in_time_is_reported() {
        let mut guard = EgressGuard::new();
        assert!(
            guard
                .check(mid(), None, 1, 4000, true, MediaKind::Video)
                .is_none()
        );
        assert!(matches!(
            guard.check(mid(), None, 2, 1000, true, MediaKind::Video),
            Some(EgressViolation::TimestampWentBackwards {
                ts: 1000,
                previous: 4000
            })
        ));
    }

    #[test]
    fn a_later_frame_reusing_an_earlier_timestamp_is_reported() {
        let mut guard = EgressGuard::new();
        let mut seq = 1u64;
        for frame in 0..10u64 {
            assert!(
                guard
                    .check(
                        mid(),
                        None,
                        seq,
                        1000 + frame * 3000,
                        true,
                        MediaKind::Video
                    )
                    .is_none()
            );
            seq += 1;
        }
        // The clock rewinds and then replays a timestamp already used.
        assert!(matches!(
            guard.check(mid(), None, seq, 1000, true, MediaKind::Video),
            Some(EgressViolation::TimestampWentBackwards { .. })
        ));
    }

    #[test]
    fn every_packet_of_one_frame_shares_its_timestamp_without_complaint() {
        let mut guard = EgressGuard::new();
        for seq in 1..=30u64 {
            let ts = 1000 + (seq / 10) * 3000;
            assert!(
                guard
                    .check(mid(), None, seq, ts, true, MediaKind::Video)
                    .is_none()
            );
        }
    }

    /// Emits `packets` packets of one frame, marking the last unless `truncated`.
    fn frame(
        guard: &mut EgressGuard,
        seq: &mut u64,
        ts: u64,
        packets: u64,
        truncated: bool,
    ) -> Option<EgressViolation> {
        let mut last = None;
        for i in 0..packets {
            let marker = !truncated && i == packets - 1;
            last = guard.check(mid(), None, *seq, ts, marker, MediaKind::Video);
            *seq += 1;
        }
        last
    }

    #[test]
    fn a_frame_cut_short_without_a_gap_is_reported() {
        let mut guard = EgressGuard::new();
        let mut seq = 1u64;
        assert!(frame(&mut guard, &mut seq, 1000, 3, false).is_none());
        assert!(frame(&mut guard, &mut seq, 4000, 2, true).is_none());
        // The next frame follows contiguously, hiding the truncation.
        assert!(matches!(
            guard.check(mid(), None, seq, 7000, false, MediaKind::Video),
            Some(EgressViolation::TruncatedFrameLooksComplete { ts: 4000, .. })
        ));
    }

    #[test]
    fn a_frame_cut_short_but_followed_by_a_gap_is_not_reported() {
        let mut guard = EgressGuard::new();
        let mut seq = 1u64;
        assert!(frame(&mut guard, &mut seq, 1000, 3, false).is_none());
        assert!(frame(&mut guard, &mut seq, 4000, 2, true).is_none());
        // One sequence number burned: the subscriber sees the damage.
        seq += 1;
        assert!(
            guard
                .check(mid(), None, seq, 7000, false, MediaKind::Video)
                .is_none(),
            "a signalled truncation is the correct behaviour, not a defect"
        );
    }

    #[test]
    fn a_frame_whose_marker_arrived_out_of_order_is_not_reported() {
        let mut guard = EgressGuard::new();
        let mut seq = 1u64;
        assert!(frame(&mut guard, &mut seq, 1000, 2, false).is_none());
        // Frame 4000: the marker packet is emitted before an earlier packet of
        // the same frame catches up.
        assert!(
            guard
                .check(mid(), None, seq + 1, 4000, true, MediaKind::Video)
                .is_none()
        );
        assert!(
            guard
                .check(mid(), None, seq, 4000, false, MediaKind::Video)
                .is_none()
        );
        seq += 2;
        assert!(
            guard
                .check(mid(), None, seq, 7000, false, MediaKind::Video)
                .is_none(),
            "the frame was closed, just not in sequence order"
        );
    }

    /// Opus packets are whole frames and normally carry no marker bit, so the
    /// video frame-completion rule must not be applied to them.
    #[test]
    fn an_ordinary_audio_stream_reports_nothing() {
        let mut guard = EgressGuard::new();
        for i in 0..200u64 {
            // 20ms Opus frames at 48kHz, marker only on the first packet.
            let violation = guard.check(
                mid(),
                None,
                5000 + i,
                1_000_000 + i * 960,
                i == 0,
                MediaKind::Audio,
            );
            assert!(
                violation.is_none(),
                "audio packet {i} reported {violation:?}"
            );
        }
    }

    #[test]
    fn audio_still_reports_a_reused_timestamp() {
        let mut guard = EgressGuard::new();
        for i in 0..10u64 {
            assert!(
                guard
                    .check(mid(), None, i, 1000 + i * 960, false, MediaKind::Audio)
                    .is_none()
            );
        }
        assert!(matches!(
            guard.check(mid(), None, 100, 1000, false, MediaKind::Audio),
            Some(EgressViolation::TimestampWentBackwards { .. })
        ));
    }

    #[test]
    fn streams_are_tracked_independently() {
        let mut guard = EgressGuard::new();
        let other = Mid::from("v1");
        assert!(
            guard
                .check(mid(), None, 1, 1000, true, MediaKind::Video)
                .is_none()
        );
        assert!(
            guard
                .check(other, None, 1, 1000, true, MediaKind::Video)
                .is_none()
        );
    }
}
