pub(crate) mod allocation;
pub(crate) mod batcher;
mod data;
pub mod direct_core;
pub mod direct_transport;
pub(crate) mod downstream;
pub mod effect;
pub(crate) mod event;
pub(crate) mod intent;
pub mod packet;
pub(crate) mod reverse;
mod signaling;
mod upstream;

use crate::{id::ShardId, keys::TrackKey, rtp::cache::TrackStreamCache};
use pulsebeam_runtime::net::RecvPacketBatch;
use tokio::time::Instant;

pub use direct_core::{DirectParticipantCore as ParticipantCore, DisconnectReason};
pub use effect::ParticipantEffect;
pub use packet::{ForwardPacket, RoutedTrackPacket, TrackPacket, TrackPacketRef};

pub struct ParticipantConfig {
    pub manual_sub: bool,
    pub room_id: crate::entity::RoomId,
    pub participant_id: crate::entity::ParticipantId,
    pub peer: pulsebeam_rtc::RtcPeer,
    pub media: Box<[pulsebeam_rtc::NegotiatedMedia]>,
}

impl std::fmt::Debug for ParticipantConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ParticipantConfig")
            .field("manual_sub", &self.manual_sub)
            .field("room_id", &self.room_id)
            .field("participant_id", &self.participant_id)
            .field("rtc_state", &self.peer.state())
            .field("media", &self.media)
            .finish_non_exhaustive()
    }
}

pub(crate) enum ParticipantInput<'a> {
    Network {
        batch: RecvPacketBatch,
        source_shard: ShardId,
    },
    Timeout(Instant),
    Track {
        now: Instant,
        key: TrackKey,
        packet: TrackPacketRef<'a>,
        cache: Option<&'a TrackStreamCache>,
    },
    Reverse {
        stream: TrackKey,
        packet: reverse::ReversePacket,
    },
}
