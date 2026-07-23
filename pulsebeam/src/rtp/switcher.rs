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
    pending: VecDeque<RtpPacket>,
    staging: KeyframeBuffer,
    latest_playout: Instant,
    seen_first: bool,
    is_switching: bool,
    switched: bool,
    /// Playout time of the in-flight old-stream frame at the moment staging
    /// triggered.  Used to distinguish same-frame late arrivals (≤ this time,
    /// forward) from next-frame old-stream packets (> this time, drop).
    old_stream_playout: Instant,
}

impl Switcher {
    pub fn new<R: RngCore>(clock_rate: Frequency, rng: &mut R) -> Self {
        let now = Instant::now();
        Self {
            timeline: Timeline::new(clock_rate, rng),
            pending: VecDeque::with_capacity(SWITCHER_PENDING_CAPACITY),
            staging: KeyframeBuffer::new(),
            latest_playout: now,
            seen_first: false,
            is_switching: false,
            switched: false,
            old_stream_playout: now,
        }
    }

    /// Pushes a packet belonging to the current `active` layer (`Slot::process`
    /// in video.rs — the ordinary steady-state path, and also the pre-promotion
    /// window where `active` is still the layer about to be replaced). Must
    /// NOT reject once the timeline has rebased (`seen_first`): after
    /// promotion, `active`-routed packets are the NEW stream's own ongoing,
    /// perfectly legitimate traffic, and `seen_first` stays latched true
    /// indefinitely between switches — rejecting on it here would silently
    /// drop all forwarding for the rest of the stream's life.
    pub fn push(&mut self, pkt: RtpPacket) {
        self.push_inner(pkt, false);
    }

    /// Pushes a late/reordered packet for the layer `active` was just
    /// promoted away from (`Slot::draining` in video.rs). Returns whether it
    /// was accepted — `false` means this switcher has nothing more to do
    /// with that old layer (either the timeline has already rebased onto the
    /// new stream, so mapping this packet's seq_no/rtp_ts through the new
    /// offsets would corrupt output, or it's from a frame that started after
    /// the switch decision) and the caller should stop routing packets for
    /// it here.
    pub fn push_draining(&mut self, pkt: RtpPacket) -> bool {
        self.push_inner(pkt, true)
    }

    fn push_inner(&mut self, pkt: RtpPacket, bar_if_rebased: bool) -> bool {
        if bar_if_rebased && self.seen_first {
            return false;
        }
        // While still switching (but before the timeline has rebased): drop
        // packets from any frame that started after the switch decision —
        // they belong to an old-stream frame we will never complete.
        if self.is_switching && pkt.playout_time > self.old_stream_playout {
            return false;
        }

        if self.pending.len() == SWITCHER_PENDING_CAPACITY {
            let _ = self.pending.pop_front();
            self.timeline.drop_count(1);
        }

        self.pending.push_back(pkt);
        self.switched = false;
        true
    }

    pub fn stage(&mut self, pkt: RtpPacket) {
        if self.is_switching {
            debug_assert!(false, "stage while state is already switching");
            return;
        }

        let is_switching = self.staging.push(pkt, self.latest_playout);
        if is_switching {
            self.seen_first = false;
            self.is_switching = true;
            // Snapshot the playout time of the in-flight old-stream frame so
            // push() can tell same-frame late arrivals from next-frame packets.
            self.old_stream_playout = self
                .pending
                .back()
                .map_or(self.latest_playout, |p| p.playout_time);
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
    fn pending_packets_are_drained_before_new_stream_on_switch() {
        let now = Instant::now();
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(12));

        switcher.push(pkt(10, 1, 1_000, now, false, true));
        let _ = switcher.pop();

        // Old stream has an in-flight incomplete frame (no marker yet).
        switcher.push(pkt(
            10,
            2,
            1_100,
            now + Duration::from_millis(10),
            false,
            false,
        ));
        switcher.push(pkt(
            10,
            3,
            1_200,
            now + Duration::from_millis(10),
            false,
            true,
        ));

        // New stream keyframe triggers switching while pending is non-empty.
        switcher.stage(pkt(
            20,
            99,
            2_000,
            now + Duration::from_millis(10),
            false,
            true,
        ));
        switcher.stage(pkt(
            20,
            100,
            2_100,
            now + Duration::from_millis(11),
            true,
            false,
        ));

        let out = drain_all(&mut switcher);
        // Old stream packets must be emitted first, then the new stream keyframe.
        assert_eq!(
            out.len(),
            3,
            "two old-stream packets then one new-stream packet"
        );
        assert_eq!(out[0].ssrc, Ssrc::from(10));
        assert_eq!(out[1].ssrc, Ssrc::from(10));
        assert!(out[1].marker, "old-stream marker must be forwarded");
        assert_eq!(out[2].ssrc, Ssrc::from(20));
    }

    #[test]
    fn late_arriving_same_frame_packets_are_forwarded_before_new_stream() {
        let now = Instant::now();
        let frame_time = now + Duration::from_millis(33);
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(13));

        // Prime an earlier complete frame to establish latest_playout.
        switcher.push(pkt(10, 0, 900, now, false, true));
        let _ = switcher.pop();

        // First packet of the in-flight frame arrives before staging triggers.
        switcher.push(pkt(10, 1, 1_000, frame_time, false, false));

        // New stream triggers; old_stream_playout is snapshotted to frame_time.
        switcher.stage(pkt(20, 99, 2_000, frame_time, false, true));
        switcher.stage(pkt(
            20,
            100,
            2_100,
            frame_time + Duration::from_millis(1),
            true,
            false,
        ));
        // is_switching=true, old_stream_playout=frame_time, pending=[pkt1]

        // Remaining packets of the SAME frame arrive late (same playout_time).
        switcher.push(pkt(10, 2, 1_100, frame_time, false, false));
        switcher.push(pkt(10, 3, 1_200, frame_time, false, true));

        let out = drain_all(&mut switcher);
        assert_eq!(
            out.len(),
            4,
            "three old-stream packets then the new-stream keyframe"
        );
        assert_eq!(out[0].ssrc, Ssrc::from(10));
        assert_eq!(out[1].ssrc, Ssrc::from(10));
        assert_eq!(out[2].ssrc, Ssrc::from(10));
        assert!(out[2].marker, "old-stream frame marker must be forwarded");
        assert_eq!(out[3].ssrc, Ssrc::from(20));
    }

    #[test]
    fn next_frame_old_stream_packets_are_dropped_after_switch() {
        let now = Instant::now();
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(14));

        // Old stream: current frame is complete (marker emitted).
        switcher.push(pkt(10, 1, 1_000, now, false, true));
        let _ = switcher.pop();

        // Staging triggers; pending is empty so old_stream_playout=latest_playout=now.
        switcher.stage(pkt(20, 99, 2_000, now, false, true));
        switcher.stage(pkt(
            20,
            100,
            2_100,
            now + Duration::from_millis(1),
            true,
            false,
        ));

        // Old stream's NEXT frame (playout_time > now): must be dropped.
        switcher.push(pkt(
            10,
            2,
            1_100,
            now + Duration::from_millis(33),
            false,
            false,
        ));
        switcher.push(pkt(
            10,
            3,
            1_200,
            now + Duration::from_millis(33),
            false,
            true,
        ));

        let out = drain_all(&mut switcher);
        assert_eq!(
            out.len(),
            1,
            "only the new-stream keyframe; next old-stream frame dropped"
        );
        assert_eq!(out[0].ssrc, Ssrc::from(20));
    }

    #[test]
    fn old_stream_packets_are_rejected_once_timeline_has_rebased() {
        let now = Instant::now();
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(15));

        switcher.push(pkt(10, 1, 1_000, now, false, true));
        let _ = switcher.pop();

        // Staging triggers and fully drains in one shot: is_switching flips
        // back to false as soon as pop() exhausts the currently-buffered
        // staging segment (a single-packet keyframe here) — this can happen
        // long before every in-flight old-stream packet has actually
        // arrived over the network.
        switcher.stage(pkt(20, 99, 2_000, now, false, true));
        switcher.stage(pkt(
            20,
            100,
            2_100,
            now + Duration::from_millis(1),
            true,
            false,
        ));
        let drained = drain_all(&mut switcher);
        assert_eq!(
            drained.len(),
            1,
            "the staged keyframe segment drains immediately"
        );

        // A late old-stream packet from the SAME in-flight frame (playout
        // time <= old_stream_playout) arrives only now, after the timeline
        // has already rebased onto stream 20's numbering (seen_first=true).
        // In the real video.rs flow this arrives via `Slot::draining`, which
        // calls `push_draining` — not the ordinary `push` used for the
        // active layer's own (now-new-stream) traffic. Forwarding it here
        // would map its seq_no/rtp_ts through the NEW stream's
        // seq_base/ts_base, corrupting the output — must be rejected
        // regardless of playout_time.
        let accepted = switcher.push_draining(pkt(10, 2, 1_100, now, false, true));
        assert!(
            !accepted,
            "old-stream packet arriving after the timeline has rebased must be rejected"
        );
        assert!(
            switcher.pop().is_none(),
            "no corrupted packet should be forwarded"
        );
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
