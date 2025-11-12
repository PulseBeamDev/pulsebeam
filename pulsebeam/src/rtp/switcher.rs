use std::time::{Duration, Instant};

use str0m::media::MediaTime;

/// Aligns a single RTP stream to a reference timeline.
pub struct Timeline {
    rtp_offset: i64,
    clock_offset: Duration,
    first_packet_seen: bool,
}

impl Timeline {
    pub fn new() -> Self {
        Self {
            rtp_offset: 0,
            clock_offset: Duration::ZERO,
            first_packet_seen: false,
        }
    }

    /// Aligns a packet to the reference RTP and wall-clock timeline.
    pub fn align<P: crate::Packet>(
        &mut self,
        pkt: &P,
        reference_rtp: MediaTime,
        reference_arrival: Instant,
    ) -> (MediaTime, Instant) {
        if !self.first_packet_seen {
            self.rtp_offset = reference_rtp.0 - pkt.rtp_timestamp().0;
            self.clock_offset = reference_arrival - pkt.arrival_timestamp();
            self.first_packet_seen = true;
        }

        let aligned_rtp = MediaTime(pkt.rtp_timestamp().0 + self.rtp_offset);
        let aligned_arrival = pkt.arrival_timestamp() + self.clock_offset;

        (aligned_rtp, aligned_arrival)
    }
}
