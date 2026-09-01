use crate::keys::TrackKey;
use crate::rtp::RtpPacket;
use crate::track::DataLane;

#[allow(
    clippy::large_enum_variant,
    reason = "RTP packets stay inline to keep the forwarding hot path allocation-free"
)]
#[derive(Debug, Clone)]
pub enum TrackPacket {
    Data { lane: DataLane, bytes: Vec<u8> },
    Rtp(RtpPacket),
}

pub enum TrackPacketRef<'a> {
    Data { lane: DataLane, bytes: &'a [u8] },
    Rtp(&'a RtpPacket),
}

impl TrackPacket {
    pub fn as_ref(&self) -> TrackPacketRef<'_> {
        match self {
            Self::Data { lane, bytes } => TrackPacketRef::Data { lane: *lane, bytes },
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
            TrackPacket::Data { .. } => {
                debug_assert!(false, "a data packet cannot enter the RTP path");
                None
            }
        }
    }
}
