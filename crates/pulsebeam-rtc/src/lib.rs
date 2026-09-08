mod id;
mod io;
mod media_packet;
mod session;
mod time;

use std::{cell::Cell, marker::PhantomData};

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

pub struct Connection {
    _time: time::MonotonicObserver,
    _not_sync: PhantomData<Cell<()>>,
}

pub struct AcceptedConnection {
    pub connection: Connection,
    pub answer: SdpAnswer,
    pub session: SessionInfo,
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;

    #[test]
    fn connection_owns_a_monotonic_observer() {
        let mut connection = Connection {
            _time: time::MonotonicObserver::default(),
            _not_sync: PhantomData,
        };
        let at = TimePoint {
            monotonic: Instant::now(),
            global: GlobalMediaTime::from_micros(1),
        };
        assert_eq!(connection._time.observe(at).global, at.global);
    }

    #[test]
    fn entropy_is_consumed_as_exactly_32_bytes() {
        assert_eq!(ConnectionEntropy::new([7; 32]).into_bytes(), [7; 32]);
    }
}
