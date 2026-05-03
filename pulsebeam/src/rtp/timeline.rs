use crate::rtp::RtpPacket;
use pulsebeam_runtime::rand::RngCore;
use str0m::{
    media::{Frequency, MediaTime},
    rtp::SeqNo,
};
use tokio::time::Instant;

#[derive(Debug)]
pub struct Timeline {
    clock_rate: Frequency,
    highest_seq_no: SeqNo,
    offset_seq_no: u64,
    anchor_playout: Option<Instant>,
    base_ts: u64,
}

impl Timeline {
    pub fn new_with_base(clock_rate: Frequency, base_seq_no: u16, base_ts: u32) -> Self {
        Self {
            clock_rate,
            highest_seq_no: SeqNo::from(base_seq_no as u64),
            offset_seq_no: 0,
            anchor_playout: None,
            base_ts: base_ts as u64,
        }
    }

    pub fn new<R: RngCore>(clock_rate: Frequency, rng: &mut R) -> Self {
        Self::new_with_base(clock_rate, rng.next_u32() as u16, rng.next_u32())
    }

    pub fn rebase(&mut self, packet: &RtpPacket) {
        let target_seq_no = self.highest_seq_no.wrapping_add(1);
        self.offset_seq_no = target_seq_no.wrapping_sub(*packet.seq_no);

        if self.anchor_playout.is_none() {
            self.anchor_playout = Some(packet.playout_time);
        }
    }

    pub fn rewrite(&mut self, pkt: &mut RtpPacket) {
        let new_seq_u64 = pkt.seq_no.wrapping_add(self.offset_seq_no);
        pkt.seq_no = new_seq_u64.into();

        if new_seq_u64 > *self.highest_seq_no {
            self.highest_seq_no = pkt.seq_no;
        }

        let anchor = self
            .anchor_playout
            .expect("rebase must have occurred before rewriting");

        // Synthesize RTP timestamp strictly from playout_time, supporting negative deltas
        let ts = if pkt.playout_time >= anchor {
            let delta = pkt.playout_time.duration_since(anchor);
            let ticks = (delta.as_secs_f64() * self.clock_rate.get() as f64) as u64;
            self.base_ts.wrapping_add(ticks)
        } else {
            let delta = anchor.duration_since(pkt.playout_time);
            let ticks = (delta.as_secs_f64() * self.clock_rate.get() as f64) as u64;
            self.base_ts.wrapping_sub(ticks)
        };

        pkt.rtp_ts = MediaTime::new(ts, self.clock_rate);
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_simple_continuity() {
        let start_time = Instant::now();
        let mut timeline = Timeline::new_with_base(Frequency::NINETY_KHZ, 0, 100_000);

        let mut p1 = RtpPacket {
            seq_no: 100.into(),
            playout_time: start_time,
            is_keyframe_start: true,
            ..Default::default()
        };

        timeline.rebase(&p1);
        timeline.rewrite(&mut p1);
        let base_out_seq = *p1.seq_no;
        let base_out_ts = p1.rtp_ts.numer();

        let mut p2 = RtpPacket {
            seq_no: 101.into(),
            playout_time: start_time + Duration::from_millis(100),
            is_keyframe_start: false,
            ..Default::default()
        };

        timeline.rewrite(&mut p2);

        assert_eq!(*p2.seq_no, base_out_seq + 1);
        assert_eq!(p2.rtp_ts.numer().wrapping_sub(base_out_ts), 9000);
    }

    #[test]
    fn test_late_packet_ordering() {
        let start_time = Instant::now();
        let mut timeline = Timeline::new_with_base(Frequency::NINETY_KHZ, 0, 100_000);

        let mut p1 = RtpPacket {
            seq_no: 10.into(),
            playout_time: start_time + Duration::from_millis(100),
            is_keyframe_start: true,
            ..Default::default()
        };
        timeline.rebase(&p1);
        timeline.rewrite(&mut p1);

        // This packet has a playout_time before the anchor
        let mut p2 = RtpPacket {
            seq_no: 11.into(),
            playout_time: start_time + Duration::from_millis(50),
            is_keyframe_start: false,
            ..Default::default()
        };

        timeline.rewrite(&mut p2);

        // Expected: 100,000 - 4500 (50ms at 90kHz) = 95,500
        assert_eq!(p2.rtp_ts.numer(), 95_500);
    }

    #[test]
    fn test_switching_streams() {
        let start_time = Instant::now();
        let mut timeline = Timeline::new_with_base(Frequency::NINETY_KHZ, 0, 10_000);

        let mut p_a1 = RtpPacket {
            seq_no: 1000.into(),
            playout_time: start_time,
            is_keyframe_start: true,
            ..Default::default()
        };
        timeline.rebase(&p_a1);
        timeline.rewrite(&mut p_a1);
        let last_seq = *p_a1.seq_no;

        // Switch to new stream B
        let mut p_b1 = RtpPacket {
            seq_no: 5000.into(),
            playout_time: start_time + Duration::from_millis(100),
            is_keyframe_start: true,
            ..Default::default()
        };
        timeline.rebase(&p_b1);
        timeline.rewrite(&mut p_b1);

        assert_eq!(*p_b1.seq_no, last_seq + 1);
        assert_eq!(p_b1.rtp_ts.numer().wrapping_sub(p_a1.rtp_ts.numer()), 9000);
    }
}
