use crate::rtp::RtpPacket;
use std::time::Duration;
use str0m::{
    media::{Frequency, MediaTime},
    rtp::rtcp::SenderInfo,
};
use tokio::time::Instant;

/// Synchronizer maps RTP timestamps to a common SFU timeline
#[derive(Debug)]
pub struct Synchronizer {
    /// RTP clock rate (e.g., 90000 for video)
    clock_rate: Frequency,
    /// Last few sender reports for interpolation/extrapolation
    sr_history: Vec<SenderInfo>,
    /// First RTP timestamp seen mapped to SFU timeline
    rtp_offset: Option<(MediaTime, Instant)>,
}

impl Synchronizer {
    /// Create a new synchronizer for a given clock rate
    pub fn new(clock_rate: Frequency) -> Self {
        Self {
            clock_rate,
            sr_history: Vec::with_capacity(3),
            rtp_offset: None,
        }
    }

    pub fn enhance_packet(&mut self, mut packet: RtpPacket, now: Instant) -> RtpPacket {
        // --- 1. Update SR history if packet has sender report ---
        if let Some(sr) = packet.last_sender_info {
            self.sr_history.push(sr);
            if self.sr_history.len() > 3 {
                self.sr_history.remove(0);
            }

            // Initialize rtp_offset if first SR
            if self.rtp_offset.is_none() {
                self.rtp_offset = Some((sr.rtp_time, now));
            }
        }

        // --- 2. Compute SFU reference timeline (playout_time) ---
        let playout_time = if let Some((first_rtp, sfu_start)) = self.rtp_offset {
            let rtp_delta = packet.rtp_ts.numer().wrapping_sub(first_rtp.numer());
            let seconds = rtp_delta as f64 / self.clock_rate.get() as f64;
            sfu_start + Duration::from_secs_f64(seconds)
        } else {
            // First packet with no SR yet: use SFU receipt time as reference
            now
        };

        packet.playout_time = Some(playout_time);
        packet
    }

    /// Optionally, get the last sender report for logging or debugging
    pub fn last_sender_report(&self) -> Option<SenderInfo> {
        self.sr_history.last().copied()
    }

    /// Returns the SFU timeline offset (first RTP → SFU time)
    pub fn rtp_offset(&self) -> Option<(MediaTime, Instant)> {
        self.rtp_offset
    }
}
