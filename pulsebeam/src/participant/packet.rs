use crate::keys::TrackKey;
use crate::rtp::RtpPacket;

#[derive(Debug, Clone)]
pub enum TrackPacket {
    Data(Vec<u8>),
    Rtp(RtpPacket),
}

#[derive(Debug, Clone)]
pub struct RoutedTrackPacket {
    pub key: TrackKey,
    pub packet: TrackPacket,
}

#[derive(Debug, Clone)]
pub struct TrackFeedback {
    pub key: TrackKey,
    pub bytes: Vec<u8>,
}

impl RoutedTrackPacket {
    pub fn into_rtp(self) -> Option<RtpPacket> {
        match self.packet {
            TrackPacket::Rtp(packet) => Some(packet),
            TrackPacket::Data(_) => {
                debug_assert!(false, "a data packet cannot enter the RTP path");
                None
            }
        }
    }
}
