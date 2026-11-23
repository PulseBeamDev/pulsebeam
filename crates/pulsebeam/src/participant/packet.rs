use crate::rtp::RtpPacket;

pub type TrackPacket = RtpPacket;
pub type TrackPacketRef<'a> = &'a RtpPacket;
