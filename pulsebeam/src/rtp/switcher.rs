use pulsebeam_runtime::rand::RngCore;
use std::collections::VecDeque;
use str0m::media::Frequency;
use tokio::time::Instant;

use crate::rtp::RtpPacket;
use crate::rtp::buffer::KeyframeBuffer;
use crate::rtp::timeline::Timeline;

const SWITCHER_PENDING_CAPACITY: usize = 32;

#[derive(Debug)]
pub struct Switcher {
    timeline: Timeline,
    /// Packets from the OLD stream currently being drained.
    pending: VecDeque<RtpPacket>,
    /// The staging area for the NEW stream.
    staging: KeyframeBuffer,
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
            staging: KeyframeBuffer::new(),
            latest_playout: Instant::now(),
            seen_first: false,
            is_switching: false,
            switched: false,
        }
    }

    pub fn push(&mut self, pkt: RtpPacket) {
        if self.is_switching {
            debug_assert!(false, "push while state is switching");
            return;
        }

        if self.pending.len() == SWITCHER_PENDING_CAPACITY {
            // Keep output sequence continuity even when dropping oldest pending packet.
            let _ = self.pending.pop_front();
            self.timeline.drop_count(1);
        }

        self.pending.push_back(pkt);
        self.switched = false;
    }

    pub fn stage(&mut self, pkt: RtpPacket) {
        if self.is_switching {
            debug_assert!(false, "stage while state is already switching");
            return;
        }

        let is_switching = self.staging.push(pkt, self.latest_playout);
        if is_switching {
            self.pending.clear();
            self.seen_first = false;
            self.is_switching = true;
        }
    }

    pub fn pop(&mut self) -> Option<RtpPacket> {
        if let Some(mut pkt) = self.pending.pop_front() {
            self.update_latest_playout(pkt.playout_time);
            self.timeline.rewrite(&mut pkt);
            return Some(pkt);
        }

        if self.is_switching {
            if let Some(mut pkt) = self.staging.pop() {
                // If this is the very first packet of the new stream after a switch.
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
        self.staging.clear();
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

    fn pkt(
        ssrc: u32,
        seq_no: u64,
        rtp_ts: u64,
        playout_time: Instant,
        is_keyframe: bool,
        marker: bool,
    ) -> RtpPacket {
        RtpPacket {
            ssrc: Ssrc::from(ssrc),
            seq_no: seq_no.into(),
            rtp_ts: MediaTime::new(rtp_ts, Frequency::NINETY_KHZ),
            playout_time,
            is_keyframe,
            marker,
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
    fn optimistic_switch_starts_before_new_keyframe_marker_arrives() {
        let now = Instant::now();
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(7));

        // Prime active stream and timeline.
        switcher.push(pkt(10, 10, 1_000, now, false, true));
        let active_out = switcher.pop().expect("active packet should be emitted");

        // The old-frame marker permits an optimistic cutover on the first IDR
        // packet. The IDR marker has not arrived yet.
        switcher.stage(pkt(20, 99, 2_000, now, false, true));
        switcher.stage(pkt(
            20,
            100,
            2_100,
            now + Duration::from_millis(1),
            true,
            false,
        ));

        let switched = drain_all(&mut switcher);
        assert_eq!(switched.len(), 1);
        assert_eq!(switched[0].ssrc, Ssrc::from(20));
        assert!(
            !switched[0].marker,
            "the switch must forward the first IDR packet before its final marker arrives"
        );
        assert_eq!(*switched[0].seq_no, (*active_out.seq_no).wrapping_add(1));
        assert!(switcher.ready_to_switch());
    }

    #[test]
    fn does_not_switch_when_staged_playout_is_too_old() {
        let now = Instant::now();
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(8));

        // Prime latest playout from active stream.
        switcher.push(pkt(10, 1, 1_000, now, false, true));
        let _ = switcher.pop();

        let old = now - Duration::from_millis(400);
        switcher.stage(pkt(
            20,
            199,
            2_000,
            old - Duration::from_millis(1),
            false,
            true,
        ));
        switcher.stage(pkt(20, 200, 2_100, old, true, false));

        assert!(switcher.pop().is_none());
        assert!(!switcher.ready_to_switch());
    }

    #[test]
    fn clear_resets_in_flight_transition_state() {
        let now = Instant::now();
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(9));

        switcher.push(pkt(10, 1, 1_000, now, false, true));
        let _ = switcher.pop();

        switcher.stage(pkt(
            20,
            49,
            2_000,
            now + Duration::from_millis(1),
            false,
            true,
        ));
        switcher.stage(pkt(
            20,
            50,
            2_100,
            now + Duration::from_millis(1),
            true,
            false,
        ));

        switcher.clear();

        assert!(switcher.pop().is_none());
        assert!(!switcher.ready_to_switch());
    }

    #[test]
    fn pending_queue_is_bounded_under_backpressure() {
        let now = Instant::now();
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(10));

        for i in 0..(SWITCHER_PENDING_CAPACITY as u64 * 4) {
            switcher.push(pkt(10, 10_000 + i, 100_000 + i, now, false, false));
        }

        assert_eq!(switcher.pending.len(), SWITCHER_PENDING_CAPACITY);
    }

    #[test]
    fn prepare_for_staging_clears_staging_and_resets_state() {
        let now = Instant::now();
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(11));

        // Prime active path and latest playout.
        switcher.push(pkt(10, 1, 1_000, now, false, true));
        let _ = switcher.pop();

        // Start one transition and complete it so state reaches Stable.
        switcher.stage(pkt(20, 99, 2_000, now, false, true));
        switcher.stage(pkt(
            20,
            100,
            2_100,
            now + Duration::from_millis(1),
            true,
            false,
        ));
        let _ = drain_all(&mut switcher);
        assert!(switcher.ready_to_switch());

        // User requests a new staging session.
        switcher.clear_staging();
        assert!(!switcher.ready_to_switch());

        // New staging should be accepted and complete normally.
        switcher.stage(pkt(
            30,
            199,
            3_000,
            now + Duration::from_millis(2),
            false,
            true,
        ));
        switcher.stage(pkt(
            30,
            200,
            3_100,
            now + Duration::from_millis(3),
            true,
            false,
        ));
        let out = drain_all(&mut switcher);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].ssrc, Ssrc::from(30));
        assert!(switcher.ready_to_switch());
    }
}
