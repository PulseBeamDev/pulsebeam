use crate::rtp::RtpPacket;
use crate::track::DataLane;

#[allow(
    clippy::large_enum_variant,
    reason = "RTP packets stay inline to keep the forwarding hot path allocation-free"
)]
#[derive(Debug, Clone)]
pub enum TrackPacket {
    Data {
        lane: DataLane,
        filter_recipients: bool,
        target: Option<crate::entity::ConnectionId>,
        #[allow(
            clippy::disallowed_types,
            reason = "RFC254 local Link fanout shares immutable payload storage; network peers serialize it"
        )]
        bytes: bytes::Bytes,
    },
    Rtp(RtpPacket),
}

pub enum TrackPacketRef<'a> {
    Data { lane: DataLane, bytes: &'a [u8] },
    Rtp(&'a RtpPacket),
}

impl TrackPacket {
    pub(crate) fn data(lane: DataLane, payload: &[u8]) -> Self {
        Self::data_for(lane, payload, None, false)
    }

    pub(crate) fn data_for(
        lane: DataLane,
        payload: &[u8],
        target: Option<crate::entity::ConnectionId>,
        filter_recipients: bool,
    ) -> Self {
        #[allow(
            clippy::disallowed_types,
            reason = "RFC254 snapshots data once before shallow immutable local fanout"
        )]
        let bytes = bytes::Bytes::copy_from_slice(payload);
        Self::Data {
            lane,
            bytes,
            target,
            filter_recipients,
        }
    }

    pub fn as_ref(&self) -> TrackPacketRef<'_> {
        match self {
            Self::Data { lane, bytes, .. } => TrackPacketRef::Data { lane: *lane, bytes },
            Self::Rtp(packet) => TrackPacketRef::Rtp(packet),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RoutedTrackPacket {
    pub publication_id: crate::entity::PublicationKey,
    pub packet: TrackPacket,
}

impl RoutedTrackPacket {
    pub fn set_remote_timing(
        &mut self,
        playout: pulsebeam_runtime::time::Instant,
        arrival: pulsebeam_runtime::time::Instant,
    ) {
        if let TrackPacket::Rtp(packet) = &mut self.packet {
            packet.playout_time = playout;
            packet.arrival_ts = arrival;
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
