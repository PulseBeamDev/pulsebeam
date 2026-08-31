use crate::keys::TrackKey;
use crate::rtp::RtpPacket;
use crate::track::DataLane;

#[allow(
    clippy::large_enum_variant,
    reason = "RTP packets stay inline to keep the forwarding hot path allocation-free"
)]
#[derive(Debug)]
pub enum TrackPacket {
    Data {
        lane: DataLane,
        bytes: Vec<u8>,
    },
    Rtp {
        packet: RtpPacket,
        media: ForwardPacket,
    },
}

pub enum TrackPacketRef<'a> {
    Data {
        lane: DataLane,
        bytes: &'a [u8],
    },
    Rtp {
        packet: &'a RtpPacket,
        media: &'a ForwardPacket,
    },
}

#[derive(Debug)]
pub enum ForwardPacket {
    Local(pulsebeam_rtc::MediaPacket),
    Transit(pulsebeam_rtc::TransitMediaPacket),
}

impl ForwardPacket {
    pub fn packet(&self) -> &pulsebeam_rtc::MediaPacket {
        match self {
            Self::Local(packet) => packet,
            Self::Transit(packet) => packet.packet(),
        }
    }

    pub fn materialize(&self) -> pulsebeam_rtc::TransitMediaPacket {
        pulsebeam_rtc::TransitMediaPacket::materialize(self.packet())
    }
}

impl TrackPacket {
    pub fn as_ref(&self) -> TrackPacketRef<'_> {
        match self {
            Self::Data { lane, bytes } => TrackPacketRef::Data { lane: *lane, bytes },
            Self::Rtp { packet, media } => TrackPacketRef::Rtp { packet, media },
        }
    }
}

#[derive(Debug)]
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
        match &mut self.packet {
            TrackPacket::Rtp { packet, .. } => {
                packet.playout_time = playout;
                packet.arrival_ts = arrival;
                packet.rehome_extensions();
            }
            TrackPacket::Data { .. } => {}
        }
    }

    pub fn into_rtp(self) -> Option<RtpPacket> {
        match self.packet {
            TrackPacket::Rtp { packet, .. } => Some(packet),
            TrackPacket::Data { .. } => {
                debug_assert!(false, "a data packet cannot enter the RTP path");
                None
            }
        }
    }
}
