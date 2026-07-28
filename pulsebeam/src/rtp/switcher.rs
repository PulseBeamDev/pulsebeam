use pulsebeam_runtime::rand::RngCore;
use std::collections::{BTreeSet, VecDeque};
use std::time::Duration;
use str0m::media::Frequency;
use str0m::rtp::SeqNo;
use tokio::time::Instant;

use crate::rtp::RtpPacket;
use crate::rtp::timeline::Timeline;

const SWITCHER_PENDING_CAPACITY: usize = 32;

/// How long after a switch the abandoned stream may still complete frames the
/// subscriber has already been given part of.
const TAIL_DRAIN_WINDOW: Duration = Duration::from_millis(200);

/// Cap on outstanding holes tracked at once, so a long run of loss cannot grow
/// the set without bound.
const MAX_TRACKED_HOLES: usize = 256;

/// Translation for the stream a slot has just switched away from.
///
/// Its packets keep arriving for a little while — reordered, or simply still in
/// flight — and some of them belong to frames the subscriber has already been
/// given part of. Holding the old mapping lets those frames be completed instead
/// of leaving the subscriber a fragment it will either render broken or count as
/// congestion loss.
#[derive(Debug)]
struct Tail {
    seq_base: SeqNo,
    ts_base: u64,
    expires_at: Instant,
    /// The gaps this stream left behind. Scoped to the tail so a later stream
    /// can never be translated into a gap belonging to an earlier one.
    holes: BTreeSet<u64>,
}

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
    /// Highest output sequence number emitted, and the one after it.
    last_output: Option<SeqNo>,
    next_expected_output: Option<u64>,
    /// Output sequence numbers skipped and not yet filled. A packet may only be
    /// emitted into one of these after a switch, which is what makes completing
    /// the old stream's frames incapable of colliding with the new one.
    holes: BTreeSet<u64>,
    /// Output sequence number of the most recent packet that closed a frame, and
    /// the first packet of the newest frame.
    last_marker_output: Option<u64>,
    frame_start_output: Option<u64>,
    frame_ts: Option<u64>,
    tail: Option<Tail>,
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
            last_output: None,
            next_expected_output: None,
            holes: BTreeSet::new(),
            last_marker_output: None,
            frame_start_output: None,
            frame_ts: None,
            tail: None,
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
    pub fn stage_direct(&mut self, packets: impl IntoIterator<Item = RtpPacket>, now: Instant) {
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

        // The active stream was abandoned partway through a frame. Skip an
        // output sequence number so the subscriber sees a hole and discards the
        // fragment, instead of reassembling it into a frame it believes whole.
        // Hand the abandoned stream a window in which its in-flight packets can
        // still complete frames the subscriber has already seen part of.
        self.tail = Some(Tail {
            seq_base: self.timeline.seq_base(),
            ts_base: self.timeline.ts_base(),
            expires_at: now + TAIL_DRAIN_WINDOW,
            holes: self.holes.clone(),
        });

        // The frame in progress was never closed, so the new stream would follow
        // it contiguously and the subscriber would read the fragment as a whole
        // frame. Leave a hole to say otherwise — which the tail then fills if the
        // packet that closes the frame does turn up. A hole *inside* the frame
        // needs no such marker: it already says the frame is damaged.
        if self.newest_frame_left_open()
            && let Some(last) = self.last_output
        {
            self.timeline.skip_output_sequence(1);
            let reserved = (*last).wrapping_add(1);
            self.holes.insert(reserved);
            if let Some(tail) = self.tail.as_mut() {
                tail.holes.insert(reserved);
            }
        }

        self.pending.clear();
        self.seen_first = false;
        self.is_switching = true;
        self.replay_floor = None;
    }

    pub fn is_switching(&self) -> bool {
        self.is_switching
    }

    /// Whether the output currently sits on a completed frame with no holes
    /// left inside it.
    ///
    /// Only the newest frame matters. Holes further back belong to frames the
    /// subscriber has already resolved one way or the other, and waiting on
    /// those — they are usually real upstream loss and will never be filled —
    /// would stall every switch for the full grace period.
    pub fn at_clean_frame_boundary(&self) -> bool {
        if self.newest_frame_left_open() {
            return false;
        }
        let frame_start = self.frame_start_output.unwrap_or(0);
        self.holes.range(frame_start..).next().is_none()
    }

    /// Whether the highest sequence number emitted so far did not close its
    /// frame, so the next stream would continue straight on from a fragment.
    ///
    /// This asks about the newest packet in sequence order, not the most
    /// recently written one: a reordered packet filling an earlier gap does not
    /// reopen a frame that has already been closed.
    fn newest_frame_left_open(&self) -> bool {
        self.last_output
            .is_some_and(|last| self.last_marker_output != Some(*last))
    }

    fn note_emitted(&mut self, seq: SeqNo, marker: bool, ts: u64) {
        let seq_v = *seq;
        match self.next_expected_output {
            None => {}
            Some(expected) if seq_v == expected => {}
            Some(expected) if seq_v > expected => {
                // A jump this large is a switch, not loss; do not record it.
                if seq_v - expected <= MAX_TRACKED_HOLES as u64 {
                    self.holes.extend(expected..seq_v);
                }
            }
            Some(_) => {
                self.holes.remove(&seq_v);
            }
        }
        while self.holes.len() > MAX_TRACKED_HOLES {
            let lowest = *self.holes.iter().next().expect("non-empty");
            self.holes.remove(&lowest);
        }

        if self.frame_ts != Some(ts) {
            self.frame_ts = Some(ts);
            self.frame_start_output = Some(seq_v);
        }
        if self.next_expected_output.is_none_or(|e| seq_v >= e) {
            self.next_expected_output = Some(seq_v + 1);
        }
        if self.last_output.is_none_or(|last| seq_v > *last) {
            self.last_output = Some(seq);
        }
        if marker {
            self.last_marker_output = Some(seq_v);
        }
    }

    /// Forward a packet from the stream this slot just switched away from.
    ///
    /// Accepted only if it lands in a sequence number the switch left unfilled,
    /// which by construction is a slot the new stream does not and cannot use.
    /// Anything else — a packet from further back, or one that would extend the
    /// old stream past where it stopped — is dropped.
    pub fn drain_tail(&mut self, pkt: &RtpPacket, now: Instant) -> Option<RtpPacket> {
        let tail = self.tail.as_ref()?;
        if now >= tail.expires_at {
            self.tail = None;
            return None;
        }

        let output_seq = (*pkt.seq_no).wrapping_add(*tail.seq_base);
        if !tail.holes.contains(&output_seq) {
            return None;
        }
        let ts_base = tail.ts_base;
        self.tail
            .as_mut()
            .expect("checked above")
            .holes
            .remove(&output_seq);
        self.holes.remove(&output_seq);

        let mut out = pkt.clone();
        out.seq_no = output_seq.into();
        out.rtp_ts = str0m::media::MediaTime::new(
            pkt.rtp_ts.numer().wrapping_add(ts_base),
            pkt.rtp_ts.frequency(),
        );
        Some(out)
    }

    pub fn has_tail(&self) -> bool {
        self.tail.is_some()
    }

    pub fn pop(&mut self) -> Option<RtpPacket> {
        if let Some(mut pkt) = self.pending.pop_front() {
            self.timeline.rewrite(&mut pkt);
            self.note_emitted(pkt.seq_no, pkt.marker, pkt.rtp_ts.numer());
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
                self.note_emitted(pkt.seq_no, pkt.marker, pkt.rtp_ts.numer());
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
        self.tail = None;
        self.holes.clear();
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

    /// A one-packet frame: complete, so switching away from it needs no
    /// damage signal.
    fn pkt(ssrc: u32, seq_no: u64, rtp_ts: u64, playout_time: Instant) -> RtpPacket {
        RtpPacket {
            ssrc: Ssrc::from(ssrc),
            seq_no: seq_no.into(),
            rtp_ts: MediaTime::new(rtp_ts, Frequency::NINETY_KHZ),
            playout_time,
            arrival_ts: playout_time,
            is_keyframe: true,
            marker: true,
            ..Default::default()
        }
    }

    /// A packet partway through a frame.
    fn mid_frame_pkt(ssrc: u32, seq_no: u64, rtp_ts: u64, playout_time: Instant) -> RtpPacket {
        let mut p = pkt(ssrc, seq_no, rtp_ts, playout_time);
        p.marker = false;
        p
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

        switcher.stage_direct([pkt(20, 100, 2_100, now + Duration::from_millis(1))], now);

        let switched = drain_all(&mut switcher);
        assert_eq!(switched.len(), 1);
        assert_eq!(switched[0].ssrc, Ssrc::from(20));
        assert_eq!(*switched[0].seq_no, (*active_out.seq_no).wrapping_add(1));
        assert!(switcher.ready_to_switch());
    }

    #[test]
    fn switching_away_mid_frame_leaves_a_gap_the_subscriber_can_see() {
        let now = Instant::now();
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(8));

        // The active stream is partway through a frame when the switch lands.
        switcher.push(mid_frame_pkt(10, 10, 1_000, now));
        let truncated = switcher.pop().expect("active packet should be emitted");

        switcher.stage_direct([pkt(20, 100, 2_100, now + Duration::from_millis(1))], now);
        let switched = drain_all(&mut switcher);

        assert_eq!(
            *switched[0].seq_no,
            (*truncated.seq_no).wrapping_add(2),
            "one sequence number must be burned so the fragment reads as damaged"
        );
    }

    #[test]
    fn clear_resets_in_flight_transition_state() {
        let now = Instant::now();
        let mut switcher = Switcher::new(rtp::VIDEO_FREQUENCY, &mut seeded_rng(9));

        switcher.push(pkt(10, 1, 1_000, now));
        let _ = switcher.pop();

        switcher.stage_direct([pkt(20, 50, 2_100, now + Duration::from_millis(1))], now);
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

        switcher.stage_direct([pkt(20, 100, 2_100, now + Duration::from_millis(1))], now);
        let _ = drain_all(&mut switcher);
        assert!(switcher.ready_to_switch());

        switcher.clear_staging();
        assert!(!switcher.ready_to_switch());

        switcher.stage_direct([pkt(30, 200, 3_100, now + Duration::from_millis(2))], now);
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
        switcher.stage_direct(burst.clone(), t0);
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

        switcher.stage_direct(b.keyframe(3), t0);
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
