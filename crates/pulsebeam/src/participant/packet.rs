use crate::rtp::RtpPacket;

pub type TrackPacket = RtpPacket;
pub type TrackPacketRef<'a> = &'a RtpPacket;

#[derive(Debug, Clone)]
pub struct RoutedTrackPacket {
    pub publication_id: crate::entity::PublicationKey,
    pub packet: RtpPacket,
}

impl RoutedTrackPacket {
    pub fn set_remote_timing(
        &mut self,
        playout: pulsebeam_runtime::time::Instant,
        arrival: pulsebeam_runtime::time::Instant,
    ) {
        self.packet.playout_time = playout;
        self.packet.arrival_ts = arrival;
    }
}
