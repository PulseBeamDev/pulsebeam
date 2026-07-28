use pulsebeam_runtime::rand::RngCore;
use std::collections::VecDeque;
use str0m::media::Frequency;
use tokio::time::Instant;

use crate::rtp::RtpPacket;
use crate::rtp::timeline::Timeline;

const SWITCHER_PENDING_CAPACITY: usize = 32;

/// Handles RTP stream switching with seamless seq/timestamp rewriting.
///
/// Callers feed the active stream via `push` and pre-validated cache packets
/// via `stage_direct`. `pop` drains the active queue first, then the staged
/// queue, rebasing the timeline on the first staged packet so the subscriber
/// sees a continuous output sequence.
#[derive(Debug)]
pub struct Switcher {
    timeline: Timeline,
    pending: VecDeque<RtpPacket>,
    staged: VecDeque<RtpPacket>,
    latest_playout: Instant,
    seen_first: bool,
    is_switching: bool,
    switched: bool,
}

impl Switcher {
    pub fn new<R: RngCore>(clock_rate: Frequency, rng: &mut R) -> Self {
        Self {
            timeline: Timeline::new(clock_rate, rng),
            pending: VecDeque::with_capacity(SWITCHER_PENDING_CAPACITY),
            staged: VecDeque::new(),
            latest_playout: Instant::now(),
            seen_first: false,
            is_switching: false,
            switched: false,
        }
    }

    pub fn push(&mut self, pkt: RtpPacket) {
        debug_assert!(!self.is_switching, "push while state is switching");
        if self.is_switching {
            return;
        }

        if self.pending.len() == SWITCHER_PENDING_CAPACITY {
            let _ = self.pending.pop_front();
            self.timeline.drop_count(1);
        }

        self.pending.push_back(pkt);
        self.switched = false;
    }

    /// Load pre-validated packets from the shared stream cache. Drains the
    /// active pending queue so output transitions cleanly to the new stream.
    pub fn stage_direct(&mut self, packets: impl IntoIterator<Item = RtpPacket>) {
        debug_assert!(!self.is_switching);
        self.staged.extend(packets);
        if !self.staged.is_empty() {
            self.pending.clear();
            self.seen_first = false;
            self.is_switching = true;
        }
    }

    pub fn is_switching(&self) -> bool {
        self.is_switching
    }

    pub fn pop(&mut self) -> Option<RtpPacket> {
        if let Some(mut pkt) = self.pending.pop_front() {
            self.update_latest_playout(pkt.playout_time);
            self.timeline.rewrite(&mut pkt);
            return Some(pkt);
        }

        if self.is_switching {
            if let Some(mut pkt) = self.staged.pop_front() {
                if !self.seen_first {
                    self.timeline.rebase(&pkt);
                    self.seen_first = true;
                }
                self.update_latest_playout(pkt.playout_time);
                self.timeline.rewrite(&mut pkt);
                return Some(pkt);
            }
            self.is_switching = false;
            self.switched = true;
        }

        None
    }

    #[inline]
    fn update_latest_playout(&mut self, time: Instant) {
        if time > self.latest_playout {
            self.latest_playout = time;
        }
    }

    pub fn ready_to_switch(&self) -> bool {
        self.pending.is_empty() && !self.is_switching && self.switched
    }

    pub fn clear_staging(&mut self) {
        self.staged.clear();
        self.is_switching = false;
        self.seen_first = false;
        self.switched = false;
    }

    pub fn clear(&mut self) {
        self.pending.clear();
        self.clear_staging();
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::rtp;
    use pulsebeam_runtime::rand::seeded_rng;
    use std::time::Duration;
    use str0m::{
        media::{Frequency, MediaTime},
        rtp::Ssrc,
    };

    fn pkt(ssrc: u32, seq_no: u64, rtp_ts: u64, playout_time: Instant) -> RtpPacket {
        RtpPacket {
            ssrc: Ssrc::from(ssrc),
            seq_no: seq_no.into(),
            rtp_ts: MediaTime::new(rtp_ts, Frequency::NINETY_KHZ),
            playout_time,
            ..Default::default()
        }
    }

    fn drain_all(switcher: &mut Switcher) -> Vec<RtpPacket> {
        let mut out = Vec::new();
        while let Some(pkt) = switcher.pop() {
            out.push(pkt);
        }
        out
    }

    #[test]
    fn switches_to_staged_stream_with_contiguous_output_sequence() {
        let now = Instant::now();
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(7));

        switcher.push(pkt(10, 10, 1_000, now));
        let active_out = switcher.pop().expect("active packet should be emitted");

        switcher.stage_direct([pkt(20, 100, 2_100, now + Duration::from_millis(1))]);

        let switched = drain_all(&mut switcher);
        assert_eq!(switched.len(), 1);
        assert_eq!(switched[0].ssrc, Ssrc::from(20));
        assert_eq!(*switched[0].seq_no, (*active_out.seq_no).wrapping_add(1));
        assert!(switcher.ready_to_switch());
    }

    #[test]
    fn clear_resets_in_flight_transition_state() {
        let now = Instant::now();
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(9));

        switcher.push(pkt(10, 1, 1_000, now));
        let _ = switcher.pop();

        switcher.stage_direct([pkt(20, 50, 2_100, now + Duration::from_millis(1))]);
        switcher.clear();

        assert!(switcher.pop().is_none());
        assert!(!switcher.ready_to_switch());
    }

    #[test]
    fn pending_queue_is_bounded_under_backpressure() {
        let now = Instant::now();
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(10));

        for i in 0..(SWITCHER_PENDING_CAPACITY as u64 * 4) {
            switcher.push(pkt(10, 10_000 + i, 100_000 + i, now));
        }

        assert_eq!(switcher.pending.len(), SWITCHER_PENDING_CAPACITY);
    }

    #[test]
    fn clear_staging_resets_and_accepts_new_stage_direct() {
        let now = Instant::now();
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(11));

        switcher.push(pkt(10, 1, 1_000, now));
        let _ = switcher.pop();

        switcher.stage_direct([pkt(20, 100, 2_100, now + Duration::from_millis(1))]);
        let _ = drain_all(&mut switcher);
        assert!(switcher.ready_to_switch());

        switcher.clear_staging();
        assert!(!switcher.ready_to_switch());

        switcher.stage_direct([pkt(30, 200, 3_100, now + Duration::from_millis(2))]);
        let out = drain_all(&mut switcher);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].ssrc, Ssrc::from(30));
        assert!(switcher.ready_to_switch());
    }
}
