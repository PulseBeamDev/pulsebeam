use pulsebeam_runtime::rand::RngCore;
use std::collections::VecDeque;
use std::time::Duration;
use str0m::media::Frequency;
use tokio::time::Instant;

use crate::rtp::RtpPacket;
use crate::rtp::buffer::KeyframeBuffer;
use crate::rtp::timeline::Timeline;

const SWITCHER_PENDING_CAPACITY: usize = 32;

/// How long to hold an optimistically-detected new-layer segment before
/// releasing it, if the old stream's currently in-flight frame never
/// confirms complete (its marker packet never arrives). Releasing on a
/// bounded timeout instead of waiting forever means a permanently-lost
/// marker (e.g. dropped and never retransmitted) can't strand the switch
/// indefinitely. Kept short: this is purely "how much longer is the old
/// frame's own tail worth waiting for", not a general network recovery
/// budget.
const OLD_FRAME_GRACE_PERIOD: Duration = Duration::from_millis(200);

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
    /// Whether the old stream's in-flight frame (identified by
    /// `old_stream_playout`) has been confirmed complete, i.e. its marker
    /// packet has been observed. Gates releasing the staged segment (see
    /// `pop`): releasing it earlier would strand that frame's remaining
    /// packets rather than let it finish (`push_inner` keeps accepting
    /// them, unrelated to whether the new segment has been released yet).
    old_frame_complete: bool,
    /// Arrival time of the packet that triggered staging; bounds how long
    /// `pop` waits for `old_frame_complete` via `OLD_FRAME_GRACE_PERIOD`.
    switch_triggered_at: Instant,
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
            old_frame_complete: true,
            switch_triggered_at: now,
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
        // A marker packet at the in-flight frame's own playout time confirms
        // it complete, regardless of whether we still accept this specific
        // packet below -- even a late/rejected one still tells `pop` it's
        // safe to release the staged segment now instead of waiting out the
        // rest of the grace period.
        if self.is_switching
            && !self.old_frame_complete
            && pkt.marker
            && pkt.playout_time == self.old_stream_playout
        {
            self.old_frame_complete = true;
        }

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
        // Already committed to this segment: every further packet for the
        // same staging target is simply the rest of it (or its successor
        // frames), not a fresh boundary to detect -- the ring/frame state
        // that detection needs was already cleared by the initial flush, so
        // re-running it here would never match again and silently strand
        // these packets. Accepting the old stream's own concurrent tail
        // (via `push`/`push_draining`) already handles pacing the actual
        // release; staging just keeps accumulating in the meantime.
        if self.is_switching {
            self.staging.append(pkt);
            return;
        }

        let arrival_ts = pkt.arrival_ts;
        let is_switching = self.staging.push(pkt, self.latest_playout);
        if is_switching {
            self.seen_first = false;
            self.is_switching = true;
            self.switch_triggered_at = arrival_ts;
            // Snapshot the playout time of the in-flight old-stream frame so
            // push() can tell same-frame late arrivals from next-frame packets.
            self.old_stream_playout = self
                .pending
                .back()
                .map_or(self.latest_playout, |p| p.playout_time);
            // If nothing was in flight (or it was already complete), there's
            // nothing to wait for.
            self.old_frame_complete = self.pending.back().is_none_or(|p| p.marker);
        }
    }

    pub fn pop(&mut self, now: Instant) -> Option<RtpPacket> {
        if let Some(mut pkt) = self.pending.pop_front() {
            self.update_latest_playout(pkt.playout_time);
            self.timeline.rewrite(&mut pkt);
            return Some(pkt);
        }

        if self.is_switching {
            // Hold the optimistically-detected segment until the old
            // stream's in-flight frame either completes or we've given it a
            // bounded chance to -- releasing it any earlier would strand
            // that frame's remaining packets instead of letting them finish
            // (see `push_inner`'s completion tracking above).
            if !self.old_frame_complete
                && now.saturating_duration_since(self.switch_triggered_at) < OLD_FRAME_GRACE_PERIOD
            {
                return None;
            }

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

    fn drain_all(switcher: &mut Switcher, now: Instant) -> Vec<RtpPacket> {
        let mut out = Vec::new();
        while let Some(pkt) = switcher.pop(now) {
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
        let active_out = switcher.pop(now).expect("active packet should be emitted");

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

        let switched = drain_all(&mut switcher, now);
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
        let _ = switcher.pop(now);

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

        assert!(switcher.pop(now).is_none());
        assert!(!switcher.ready_to_switch());
    }

    #[test]
    fn clear_resets_in_flight_transition_state() {
        let now = Instant::now();
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(9));

        switcher.push(pkt(10, 1, 1_000, now, false, true));
        let _ = switcher.pop(now);

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

        assert!(switcher.pop(now).is_none());
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
        let _ = switcher.pop(now);

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

        let out = drain_all(&mut switcher, now);
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
        let _ = switcher.pop(now);

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

        let out = drain_all(&mut switcher, now);
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
    fn old_frame_completes_after_new_layer_is_optimistically_detected() {
        let now = Instant::now();
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(16));

        // Prime an earlier complete frame.
        switcher.push(pkt(10, 0, 900, now, false, true));
        let _ = switcher.pop(now);

        let frame_time = now + Duration::from_millis(33);
        // Old stream's next frame starts, but is NOT yet complete (no marker).
        switcher.push(pkt(10, 1, 1_000, frame_time, false, false));

        // New layer's keyframe is optimistically detected -- a separate
        // on_rtp call in the real flow, well before the old frame's marker
        // has necessarily arrived over the network.
        switcher.stage(pkt(20, 99, 2_000, frame_time, false, true));
        switcher.stage(pkt(
            20,
            100,
            2_100,
            frame_time + Duration::from_millis(1),
            true,
            false,
        ));

        // The old in-flight packet (already buffered) drains normally...
        assert_eq!(switcher.pop(now).map(|p| p.ssrc), Some(Ssrc::from(10)));
        // ...but the staged new-layer segment must NOT release yet: the old
        // frame's marker hasn't arrived, and the grace period hasn't elapsed.
        // Releasing here would strand the old frame's remaining packet
        // instead of letting it finish.
        assert!(switcher.pop(now).is_none());
        assert!(!switcher.ready_to_switch());

        // More of the new layer's keyframe arrives via a separate on_rtp
        // call while still holding. `stage`'s boundary-detection no longer
        // applies once `is_switching` is already true (the ring/frame state
        // it needs was cleared by the initial flush) -- this packet must be
        // accumulated via `append`, not silently dropped.
        switcher.stage(pkt(
            20,
            101,
            2_200,
            frame_time + Duration::from_millis(1),
            true,
            false,
        ));

        // The old stream's frame finally completes.
        switcher.push(pkt(10, 2, 1_100, frame_time, false, true));

        let out = drain_all(&mut switcher, now);
        assert_eq!(
            out.len(),
            3,
            "old frame's marker packet, then both buffered new-layer packets"
        );
        assert_eq!(out[0].ssrc, Ssrc::from(10));
        assert!(out[0].marker, "old-stream frame marker must be forwarded");
        assert_eq!(out[1].ssrc, Ssrc::from(20));
        assert_eq!(out[2].ssrc, Ssrc::from(20));
        assert!(switcher.ready_to_switch());
    }

    #[test]
    fn old_frame_grace_period_releases_the_staged_segment_if_marker_never_arrives() {
        let now = Instant::now();
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(17));

        switcher.push(pkt(10, 0, 900, now, false, true));
        let _ = switcher.pop(now);

        let frame_time = now + Duration::from_millis(33);
        // This packet never gets a marker -- simulates a permanently lost
        // final packet for the old stream's in-flight frame.
        switcher.push(pkt(10, 1, 1_000, frame_time, false, false));

        switcher.stage(pkt(20, 99, 2_000, frame_time, false, true));
        switcher.stage(pkt(
            20,
            100,
            2_100,
            frame_time + Duration::from_millis(1),
            true,
            false,
        ));

        let _ = switcher.pop(now);
        assert!(
            switcher.pop(now).is_none(),
            "must hold before the grace period elapses"
        );

        let after_grace = now + OLD_FRAME_GRACE_PERIOD + Duration::from_millis(1);
        let released = switcher
            .pop(after_grace)
            .expect("segment releases once the grace period elapses, marker or not");
        assert_eq!(released.ssrc, Ssrc::from(20));
    }

    #[test]
    fn next_frame_old_stream_packets_are_dropped_after_switch() {
        let now = Instant::now();
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(14));

        // Old stream: current frame is complete (marker emitted).
        switcher.push(pkt(10, 1, 1_000, now, false, true));
        let _ = switcher.pop(now);

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

        let out = drain_all(&mut switcher, now);
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
        let _ = switcher.pop(now);

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
        let drained = drain_all(&mut switcher, now);
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
            switcher.pop(now).is_none(),
            "no corrupted packet should be forwarded"
        );
    }

    #[test]
    fn prepare_for_staging_clears_staging_and_resets_state() {
        let now = Instant::now();
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(11));

        // Prime active path and latest playout.
        switcher.push(pkt(10, 1, 1_000, now, false, true));
        let _ = switcher.pop(now);

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
        let _ = drain_all(&mut switcher, now);
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
        let out = drain_all(&mut switcher, now);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].ssrc, Ssrc::from(30));
        assert!(switcher.ready_to_switch());
    }
}
