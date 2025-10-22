pub mod jitter_buffer;
pub mod rtp_rewriter;
#[cfg(test)]
pub mod test;

use std::ops::{Deref, DerefMut};

use str0m::{media::MediaTime, rtp::SeqNo};
use tokio::time::Instant;

#[derive(Debug)]
pub struct RtpPacket {
    inner: str0m::rtp::RtpPacket,
}

impl Deref for RtpPacket {
    type Target = str0m::rtp::RtpPacket;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for RtpPacket {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl From<str0m::rtp::RtpPacket> for RtpPacket {
    fn from(value: str0m::rtp::RtpPacket) -> Self {
        Self { inner: value }
    }
}

impl AsRef<str0m::rtp::RtpPacket> for RtpPacket {
    fn as_ref(&self) -> &str0m::rtp::RtpPacket {
        &self.inner
    }
}

pub trait PacketTiming {
    fn seq_no(&self) -> SeqNo;
    fn rtp_timestamp(&self) -> MediaTime;
    fn arrival_timestamp(&self) -> Instant;
}

impl PacketTiming for RtpPacket {
    fn seq_no(&self) -> SeqNo {
        self.inner.seq_no
    }
    fn rtp_timestamp(&self) -> MediaTime {
        self.inner.time
    }
    fn arrival_timestamp(&self) -> Instant {
        self.inner.timestamp.into()
    }
}
