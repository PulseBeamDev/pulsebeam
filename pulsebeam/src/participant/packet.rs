use crate::keys::TrackKey;
use crate::rtp::RtpPacket;

#[derive(Debug, Clone)]
pub struct TrackPacket {
    pub key: TrackKey,
    pub packet: Box<RtpPacket>,
}
