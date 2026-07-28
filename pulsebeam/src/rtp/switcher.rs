use pulsebeam_runtime::rand::RngCore;
use std::collections::VecDeque;
use str0m::media::Frequency;
use str0m::rtp::SeqNo;

use crate::rtp::RtpPacket;
use crate::rtp::timeline::Timeline;

const SWITCHER_PENDING_CAPACITY: usize = 32;

/// Handles RTP stream switching with seamless seq/timestamp rewriting.
///
/// Callers feed the active stream via `push` and a pre-validated switch segment
/// via `stage_direct`. `pop` drains the active queue first, then the staged
/// segment, rebasing the timeline on the first staged packet so the subscriber
/// sees one continuous output stream.
///
/// The staged segment is emitted onto consecutive output sequence numbers: the
/// cache has already ordered and deduplicated it, and it may carry synthesized
/// parameter-set packets whose original sequence numbers are unrelated. Live
/// packets resume the stream's own numbering afterwards so later upstream loss
/// still reaches the subscriber as a gap.
#[derive(Debug)]
pub struct Switcher {
    timeline: Timeline,
    pending: VecDeque<RtpPacket>,
    staged: VecDeque<RtpPacket>,
    /// Highest input seq emitted from the staged burst. Live packets at or below
    /// it were already sent and must not be emitted a second time.
    replay_floor: Option<SeqNo>,
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
            replay_floor: None,
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

        // Already delivered as part of the switch burst.
        if self.replay_floor.is_some_and(|floor| *pkt.seq_no <= *floor) {
            return;
        }

        if self.pending.len() == SWITCHER_PENDING_CAPACITY {
            // Callers drain after every push, so this is unreachable in practice.
            // Drop rather than close the gap: silently renumbering around a lost
            // packet leaves the subscriber decoding a hole it cannot detect.
            debug_assert!(false, "switcher pending queue overflowed");
            let _ = self.pending.pop_front();
        }

        self.pending.push_back(pkt);
        self.switched = false;
    }

    /// Load a switch segment produced by `StreamCache::replay`.
    ///
    /// The segment must be ordered by sequence number and start a decodable
    /// frame; the cache is responsible for both.
    pub fn stage_direct(&mut self, packets: impl IntoIterator<Item = RtpPacket>) {
        debug_assert!(!self.is_switching);
        debug_assert!(
            self.pending.is_empty(),
            "stage_direct must follow a full drain"
        );

        self.staged.clear();
        self.staged.extend(packets);
        if self.staged.is_empty() {
            return;
        }

        debug_assert!(
            self.staged.iter().any(|p| p.is_keyframe),
            "staged segment must be decodable on its own"
        );
        debug_assert!(
            self.staged
                .make_contiguous()
                .windows(2)
                .all(|w| *w[0].seq_no <= *w[1].seq_no),
            "staged segment must be ordered"
        );

        self.pending.clear();
        self.seen_first = false;
        self.is_switching = true;
        self.replay_floor = None;
    }

    pub fn is_switching(&self) -> bool {
        self.is_switching
    }

    pub fn pop(&mut self) -> Option<RtpPacket> {
        if let Some(mut pkt) = self.pending.pop_front() {
            self.timeline.rewrite(&mut pkt);
            return Some(pkt);
        }

        if self.is_switching {
            if let Some(mut pkt) = self.staged.pop_front() {
                if !self.seen_first {
                    self.timeline.rebase(&pkt);
                    self.seen_first = true;
                }
                let input_seq = pkt.seq_no;
                self.timeline.rewrite_sequential(&mut pkt);
                if self.replay_floor.is_none_or(|floor| *input_seq > *floor) {
                    self.replay_floor = Some(input_seq);
                }
                return Some(pkt);
            }

            // Burst complete: realign so the stream's own numbering resumes.
            if let Some(floor) = self.replay_floor {
                self.timeline.resync_to_input(floor);
            }
            self.is_switching = false;
            self.switched = true;
        }

        None
    }

    pub fn ready_to_switch(&self) -> bool {
        self.pending.is_empty() && !self.is_switching && self.switched
    }

    pub fn clear_staging(&mut self) {
        self.staged.clear();
        self.is_switching = false;
        self.seen_first = false;
        self.switched = false;
        self.replay_floor = None;
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
    use crate::rtp::test_utils::{H264StreamBuilder, ParameterSetStyle};
    use pulsebeam_runtime::rand::seeded_rng;
    use std::time::Duration;
    use str0m::{
        media::{Frequency, MediaTime},
        rtp::Ssrc,
    };
    use tokio::time::Instant;

    fn pkt(ssrc: u32, seq_no: u64, rtp_ts: u64, playout_time: Instant) -> RtpPacket {
        RtpPacket {
            ssrc: Ssrc::from(ssrc),
            seq_no: seq_no.into(),
            rtp_ts: MediaTime::new(rtp_ts, Frequency::NINETY_KHZ),
            playout_time,
            arrival_ts: playout_time,
            is_keyframe: true,
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

    #[test]
    fn a_replayed_burst_is_emitted_contiguously_and_never_twice() {
        let t0 = Instant::now();
        let mut b = H264StreamBuilder::new(9, 500, 90_000, t0)
            .with_parameter_sets(ParameterSetStyle::SeparatePacket);
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(12));

        switcher.push(pkt(1, 5, 1_000, t0));
        let last_active = switcher.pop().unwrap();

        let burst = b.keyframe(3);
        let burst_last_seq = *burst.last().unwrap().seq_no;
        switcher.stage_direct(burst.clone());
        let out = drain_all(&mut switcher);

        assert_eq!(out.len(), burst.len());
        assert_eq!(*out[0].seq_no, *last_active.seq_no + 1);
        assert!(out.windows(2).all(|w| *w[1].seq_no == *w[0].seq_no + 1));

        // Reordering redelivers a packet that was already in the burst.
        for p in burst.iter().rev() {
            switcher.push(p.clone());
        }
        assert!(
            drain_all(&mut switcher).is_empty(),
            "packets already sent in the burst must not be emitted again"
        );

        // The next genuinely-new live packet continues seamlessly.
        let live = b.delta_frame(1);
        assert_eq!(*live[0].seq_no, burst_last_seq + 1);
        switcher.push(live[0].clone());
        let out2 = drain_all(&mut switcher);
        assert_eq!(*out2[0].seq_no, *out.last().unwrap().seq_no + 1);
    }

    #[test]
    fn live_loss_after_a_switch_still_reaches_the_subscriber_as_a_gap() {
        let t0 = Instant::now();
        let mut b = H264StreamBuilder::new(9, 500, 90_000, t0)
            .with_parameter_sets(ParameterSetStyle::SeparatePacket);
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(13));

        switcher.stage_direct(b.keyframe(3));
        let out = drain_all(&mut switcher);
        let last_out = *out.last().unwrap().seq_no;

        b.drop_packets(4);
        let f = b.delta_frame(1);
        switcher.push(f[0].clone());
        let out2 = drain_all(&mut switcher);

        assert_eq!(
            *out2[0].seq_no - last_out,
            5,
            "4 lost upstream packets must leave a detectable hole"
        );
    }
}
