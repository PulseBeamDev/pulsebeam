mod connection;
mod id;
mod io;
mod media_packet;
mod negotiation;
mod session;
mod time;

pub use connection::{AcceptedConnection, Connection};
pub use id::{DataChannelId, EncodingId, FrameId, IceTcpFlowId, SenderId};
pub use io::{
    AcceptError, AllocationSnapshot, CloseReason, Command, CommandError, ConnectionStats,
    ConnectionWarning, DataChannelEvent, DataChannelStats, EcnCodepoint, EncodingInfo,
    EncodingRetireReason, EncodingStats, Event, NetworkInput, Output, ReceiveError,
    SenderAllocation, SenderStats, StatsSnapshot, Transmit, TransmitTarget,
};
pub use media_packet::{
    ForwardedMedia, FrameBoundary, FrameDependencies, FrameMetadata, MediaPacket,
};
pub use session::{
    ConnectionConfig, ConnectionLimits, DataChannelConfig, DataChannelConfigError,
    DataChannelPriority, DataMessage, DataReliability, MediaKind, MediaPayloadBitrate,
    MediaPriority, PacketFeedbackKind, PlayoutDelay, PolicyError, SdpAnswer, SdpOffer, SenderInfo,
    SenderPolicy, SessionCapabilities, SessionInfo, UnknownExtensionPolicy,
};
pub use time::{ConnectionEntropy, GlobalMediaTime, TimePoint};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entropy_is_consumed_as_exactly_32_bytes() {
        assert_eq!(ConnectionEntropy::new([7; 32]).into_bytes(), [7; 32]);
    }
}
