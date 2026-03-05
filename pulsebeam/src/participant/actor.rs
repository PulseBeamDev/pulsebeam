use std::{collections::HashMap, sync::Arc};

use crate::participant::batcher::Batcher;
use crate::participant::core::ParticipantCore;
use crate::{entity, track};
use pulsebeam_runtime::net;
use str0m::Rtc;

pub use crate::participant::core::TrackMapping;

/// Control messages routed from the room/shard to a participant's core.
///
/// These are low-frequency signals; they travel through the shard's control
/// channel rather than a dedicated per-participant mailbox.
#[derive(Debug, Clone)]
pub enum ParticipantControlMessage {
    TracksSnapshot(HashMap<entity::TrackId, track::TrackReceiver>),
    TracksPublished(Arc<HashMap<entity::TrackId, track::TrackReceiver>>),
    TracksUnpublished(Arc<HashMap<entity::TrackId, track::TrackReceiver>>),
    TrackPublishRejected(track::TrackReceiver),
}

/// A fully-constructed participant ready to be placed into a [`crate::shard::ShardLoop`].
///
/// Replaces the old `ParticipantActor` + per-task model.  The run loop now
/// lives in the shard; this struct is purely a data bundle.
pub struct ParticipantActor {
    /// ICE username fragment used for gateway demux routing.
    pub ufrag: String,
    /// The RTC state machine and all hot-path allocators.
    pub core: ParticipantCore,
    /// Inbound UDP/TCP sender — given to the gateway demuxer so it can route
    /// network packets to this participant's entry in the shard.
    pub gateway_tx: pulsebeam_runtime::sync::mpsc::Sender<net::RecvPacketBatch>,
    /// Inbound receiver held by the shard entry.
    pub gateway_rx: pulsebeam_runtime::sync::mpsc::Receiver<net::RecvPacketBatch>,
    /// Pre-allocated UDP egress socket writer.
    pub udp_egress: net::UnifiedSocketWriter,
    /// Pre-allocated TCP egress socket writer.
    pub tcp_egress: net::UnifiedSocketWriter,
}

impl ParticipantActor {
    pub fn new(
        udp_egress: net::UnifiedSocketWriter,
        tcp_egress: net::UnifiedSocketWriter,
        participant_id: entity::ParticipantId,
        mut rtc: Rtc,
        manual_sub: bool,
    ) -> Self {
        let ufrag = rtc.direct_api().local_ice_credentials().ufrag;
        let (gateway_tx, gateway_rx) = pulsebeam_runtime::sync::mpsc::channel(256);
        let udp_batcher = Batcher::with_capacity(udp_egress.max_gso_segments());
        let tcp_batcher = Batcher::with_capacity(tcp_egress.max_gso_segments());
        let core = ParticipantCore::new(manual_sub, participant_id, rtc, udp_batcher, tcp_batcher);
        Self {
            ufrag,
            core,
            gateway_tx,
            gateway_rx,
            udp_egress,
            tcp_egress,
        }
    }

    pub fn meta(&self) -> entity::ParticipantId {
        self.core.participant_id
    }
}
