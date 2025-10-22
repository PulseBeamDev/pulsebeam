use str0m::{media::MediaTime, rtp::SeqNo};
use tokio::time::Instant;

use crate::rtp::PacketTiming;

#[derive(Clone, Copy)]
pub struct TestPacket {
    pub seq_no: SeqNo,
    pub rtp_ts: MediaTime,
    pub server_ts: Instant,
}

impl PacketTiming for TestPacket {
    fn seq_no(&self) -> SeqNo {
        self.seq_no
    }

    fn rtp_timestamp(&self) -> MediaTime {
        self.rtp_ts
    }

    fn arrival_timestamp(&self) -> Instant {
        self.server_ts
    }
}
