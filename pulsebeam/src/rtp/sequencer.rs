use str0m::media::{Frequency, MediaTime};
use str0m::rtp::{SeqNo, Ssrc};

use crate::rtp::Packet;

/// The standard 90kHz clock rate for video RTP, used for all internal timestamp math.
const VIDEO_FREQUENCY: Frequency = Frequency::NINETY_KHZ;
const AUDIO_FREQUENCY: Frequency = Frequency::FORTY_EIGHT_KHZ;

pub struct RtpSequencer<T> {
    frequency: Frequency,
    forwarded_seq_no: SeqNo,
    forwarded_rtp_ts: MediaTime,
    switching: bool,

    last_stream: Option<StreamState>,
}

impl<T: Packet> RtpSequencer<T> {
    pub fn video() -> Self {
        Self::new(VIDEO_FREQUENCY)
    }

    pub fn audio() -> Self {
        Self::new(AUDIO_FREQUENCY)
    }

    pub fn new(frequency: Frequency) -> Self {
        let start_ts = rand::random();
        Self {
            frequency,
            forwarded_seq_no: SeqNo::new(),
            forwarded_rtp_ts: MediaTime::new(start_ts, frequency),
            switching: true,

            last_stream: None,
        }
    }

    pub fn push(&mut self, packet: &T) {
        if self.switching {
            if let Some(last_state) = &self.last_stream {
            } else {
            }
        }
    }

    pub fn pop(&mut self) -> Option<(SeqNo, MediaTime, T)> {}
}

struct StreamState {
    ssrc: Ssrc,
    seq_no: SeqNo,
    rtp_ts: MediaTime,
}
