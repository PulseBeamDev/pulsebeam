mod allocator;
mod clock;
mod congestion;
mod connection;
mod dependency_descriptor;
mod egress;
mod id;
mod ingress;
mod io;
mod media_packet;
mod media_transit;
mod negotiation;
mod pacer;
mod packet;
mod receive_stream;
mod rtcp;
mod scheduler;
mod sctp;
mod sent_history;
mod session;
mod time;
mod transport;

#[cfg(test)]
extern crate self as pulsebeam_rtc;
#[cfg(test)]
mod network_tests;
#[cfg(test)]
mod test_source;
#[cfg(test)]
#[path = "../tests/support/mod.rs"]
mod test_support;

pub use connection::{AcceptedConnection, Connection};
pub use id::{DataChannelId, EncodingId, FrameId, IceTcpFlowId, SenderId};
pub use io::{
    AcceptError, AllocationSnapshot, AuthenticatedFlow, CloseReason, Command, CommandError,
    ConnectionState, ConnectionStats, ConnectionWarning, DataChannelEvent, DataChannelStats,
    EcnCodepoint, EncodingInfo, EncodingRetireReason, EncodingStats, Event, KeyframeRequestReason,
    NetworkInput, Output, PlayoutDelayStatus, ReceiveError, SenderAllocation, SenderStats,
    StatsSnapshot, Transmit, TransmitTarget,
};
pub use media_packet::{
    ForwardedMedia, FrameBoundary, FrameDependencies, FrameMetadata, MediaPacket, MediaPacketView,
};
pub use media_transit::{MaterializedMediaPacket, MediaTransitError};
pub use session::{
    ConnectionConfig, ConnectionLimits, DataChannelConfig, DataChannelConfigError,
    DataChannelPriority, DataMessage, DataReliability, InboundMediaInfo, LocalCandidate,
    LocalIceCredentials, MediaKind, MediaPayloadBitrate, MediaPriority, PacketFeedbackKind,
    PlayoutDelay, PolicyError, SdpAnswer, SdpOffer, SenderDemand, SenderInfo, SenderPolicy,
    SessionCapabilities, SessionInfo, UnknownExtensionPolicy,
};
pub use time::{ConnectionEntropy, GlobalMediaTime, TimePoint};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_native_fixture_contracts() {
        crate::test_support::scenario_clock_binding_survives_later_peer_initialization();
        crate::test_support::application_only_fixture_has_no_media();
        crate::test_support::fixture_advance_stops_at_requested_deadline();
        crate::test_support::wire_observer_reads_two_byte_dependency_descriptor();
        crate::test_support::wire_observer_uses_negotiated_extension_ids();
    }

    #[test]
    fn entropy_is_consumed_as_exactly_32_bytes() {
        assert_eq!(ConnectionEntropy::new([7; 32]).into_bytes(), [7; 32]);
    }
}
