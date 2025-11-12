use std::time::{Duration, Instant};
use str0m::media::{Frequency, MediaTime};

/// Optional RTCP Sender Report info included in RTP packets
#[derive(Debug, Clone, Copy)]
pub struct SenderInfo {
    /// RTP timestamp of the sender report
    pub rtp: MediaTime,
    /// Optional NTP/SFU time corresponding to the RTP timestamp
    pub ntp: Option<Instant>,
}

/// RTP packet metadata
#[derive(Debug, Clone, Copy)]
pub struct RtpPacket {
    /// RTP timestamp of this packet
    pub rtp: MediaTime,
    /// RTP sequence number
    pub sequence: u16,
    /// Optional last sender report info
    pub last_sender_info: Option<SenderInfo>,
    /// Calculated SFU-aligned time for playback or mixing
    pub playout_time: Option<Instant>,
}

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

    /// Middleware-style processing: enhance an RTP packet with SFU timeline
    ///
    /// # Arguments
    ///
    /// * `packet` - the incoming RTP packet
    /// * `now` - SFU receipt time of this packet
    ///
    /// # Returns
    ///
    /// The same packet with `playout_time` set to the computed SFU timeline
    pub fn enhance_packet(&mut self, mut packet: RtpPacket, now: Instant) -> RtpPacket {
        // --- 1. Update SR history if packet has sender report ---
        if let Some(sr) = packet.last_sender_info {
            self.sr_history.push(sr);
            if self.sr_history.len() > 3 {
                self.sr_history.remove(0);
            }

            // Initialize rtp_offset if first SR
            if self.rtp_offset.is_none() {
                self.rtp_offset = Some((sr.rtp, now));
            }
        }

        // --- 2. Compute SFU reference timeline (playout_time) ---
        let playout_time = if let Some((first_rtp, sfu_start)) = self.rtp_offset {
            let rtp_delta = packet.rtp.numer().wrapping_sub(first_rtp.numer());
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
