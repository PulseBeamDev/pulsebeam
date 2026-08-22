use crate::keys::TrackKey;
use crate::rtp::RtpPacket;

#[derive(Debug, Clone)]
pub enum TrackPacket {
    Data(DataPacket),
    Audio(AudioPacket),
    Video(VideoPacket),
}

#[derive(Debug, Clone)]
pub struct DataPacket {
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct AudioPacket {
    pub packet: RtpPacket,
}

#[derive(Debug, Clone)]
pub struct VideoPacket {
    pub packet: RtpPacket,
}

#[derive(Debug, Clone)]
pub struct RoutedTrackPacket {
    pub key: TrackKey,
    pub packet: TrackPacket,
}

impl RoutedTrackPacket {
    pub fn into_rtp(self) -> Option<RtpPacket> {
        match self.packet {
            TrackPacket::Audio(packet) => Some(packet.packet),
            TrackPacket::Video(packet) => Some(packet.packet),
            TrackPacket::Data(_) => {
                debug_assert!(false, "a routed data packet cannot enter the RTP path");
                None
            }
        }
    }
}
