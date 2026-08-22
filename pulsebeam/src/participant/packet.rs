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
    pub packet: Box<RtpPacket>,
}

#[derive(Debug, Clone)]
pub struct VideoPacket {
    pub packet: Box<RtpPacket>,
}

#[derive(Debug, Clone)]
pub struct RoutedTrackPacket {
    pub key: TrackKey,
    pub packet: Box<RtpPacket>,
}
