use crate::keys::TrackKey;
use crate::rtp::RtpPacket;

#[derive(Debug, Clone)]
pub enum TrackPacket {
    Data(Vec<u8>),
    Reliable(Vec<u8>),
    Rtp(RtpPacket),
}

pub enum TrackPacketRef<'a> {
    Data(&'a [u8]),
    Reliable(&'a [u8]),
    Rtp(&'a RtpPacket),
}

impl TrackPacket {
    pub fn as_ref(&self) -> TrackPacketRef<'_> {
        match self {
            Self::Data(bytes) => TrackPacketRef::Data(bytes),
            Self::Reliable(bytes) => TrackPacketRef::Reliable(bytes),
            Self::Rtp(packet) => TrackPacketRef::Rtp(packet),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RoutedTrackPacket {
    pub key: TrackKey,
    pub packet: TrackPacket,
}

impl RoutedTrackPacket {
    pub fn set_remote_timing(
        &mut self,
        playout: tokio::time::Instant,
        arrival: tokio::time::Instant,
    ) {
        if let TrackPacket::Rtp(packet) = &mut self.packet {
            packet.playout_time = playout;
            packet.arrival_ts = arrival;
            packet.rehome_extensions();
        }
    }

    pub fn into_rtp(self) -> Option<RtpPacket> {
        match self.packet {
            TrackPacket::Rtp(packet) => Some(packet),
            TrackPacket::Data(_) | TrackPacket::Reliable(_) => {
                debug_assert!(false, "a data packet cannot enter the RTP path");
                None
            }
        }
    }
}
