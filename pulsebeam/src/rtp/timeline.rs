use crate::rtp::RtpPacket;
use str0m::{
    media::{Frequency, MediaTime},
    rtp::SeqNo,
};
use tokio::time::Instant;

// Timeline allows switching between input streams that
// output a stream with a continuous seq_no and playout_time
// as if it comes from a single stream.
#[derive(Debug)]
pub struct Timeline {
    clock_rate: Frequency,
    highest_seq_no: SeqNo,
    offset_seq_no: u64,
    anchor: Option<Instant>,
}

impl Timeline {
    pub fn new(clock_rate: Frequency) -> Self {
        let base_seq_no: u8 = rand::random();
        Self {
            clock_rate,
            highest_seq_no: SeqNo::from(base_seq_no as u64),
            offset_seq_no: 0,
            anchor: None,
        }
    }

    // packet is guaranteed to be the first packet after a marker and is a keyframe
    pub fn rebase(&mut self, packet: &RtpPacket) {
        debug_assert!(packet.is_keyframe_start);

        let target_seq_no = self.highest_seq_no.wrapping_add(1);
        self.offset_seq_no = target_seq_no.wrapping_sub(*packet.seq_no);

        if self.anchor.is_none() {
            self.anchor = Some(packet.playout_time);
        }
    }

    pub fn rewrite(&mut self, mut pkt: RtpPacket) -> RtpPacket {
        pkt.seq_no = pkt.seq_no.wrapping_add(self.offset_seq_no).into();
        if pkt.seq_no > self.highest_seq_no {
            self.highest_seq_no = pkt.seq_no;
        }

        let anchor = self
            .anchor
            .expect("rebase must have occured before rewriting to create the first anchor");
        pkt.rtp_ts = MediaTime::from(pkt.playout_time.saturating_duration_since(anchor))
            .rebase(self.clock_rate);
        pkt
    }
}
