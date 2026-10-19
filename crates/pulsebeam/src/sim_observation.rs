//! Simulation-only owned observations. Each producer copies its own state in
//! one turn; collection does not imply a globally atomic distributed snapshot.
use crate::{
    control::controller::ControllerCommand,
    entity::{ConnectionId, ParticipantId, RoomId, TrackId},
    id::ShardId,
    route::{NodeRouteAddress, NodeTransportAddress},
    track::{SelectionPolicy, TrackMeta, TrackSelector},
};
use pulsebeam_runtime::mailbox;
use tokio::sync::oneshot;

#[derive(Clone, Debug)]
pub struct ParticipantObservation {
    pub id: ParticipantId,
    pub room: RoomId,
    pub connection: ConnectionId,
    pub shard: ShardId,
    pub transport: Option<NodeTransportAddress>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SelectorObservation {
    pub track: Option<TrackId>,
    pub publisher: Option<ParticipantId>,
    pub kind: Option<crate::entity::TrackKind>,
    pub label: Option<String>,
}

impl From<&TrackSelector> for SelectorObservation {
    fn from(selector: &TrackSelector) -> Self {
        Self {
            track: selector.track,
            publisher: selector.publisher,
            kind: selector.kind,
            label: selector.label.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SubscriptionObservation {
    pub room: RoomId,
    pub subscriber: ParticipantId,
    pub selector: SelectorObservation,
    pub selection: SelectionPolicy,
}

#[derive(Clone, Debug)]
pub struct TrackObservation {
    pub meta: TrackMeta,
    pub candidates: Vec<ParticipantId>,
    pub active: Vec<ParticipantId>,
}

#[derive(Clone, Debug)]
pub struct PlacementObservation {
    pub track: TrackId,
    pub address: NodeRouteAddress,
    pub resident: bool,
    pub route_installed: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ControlObservation {
    pub generation: u64,
    pub shard_generations: Vec<u64>,
    pub participants: Vec<ParticipantObservation>,
    pub tracks: Vec<TrackObservation>,
    pub subscriptions: Vec<SubscriptionObservation>,
    pub placements: Vec<PlacementObservation>,
}

#[derive(Clone, Debug)]
pub struct PlanObservation {
    pub track: Option<TrackId>,
    pub local: Vec<Option<ParticipantId>>,
    pub remote: Vec<NodeRouteAddress>,
    pub reverse: Option<NodeRouteAddress>,
}

#[derive(Clone, Debug)]
pub struct RouteObservation {
    pub address: NodeRouteAddress,
    pub track: Option<TrackId>,
    pub reverse: bool,
}

#[derive(Clone, Debug)]
pub struct VideoSlotObservation {
    pub track: TrackId,
    pub origin: ParticipantId,
    pub quality: u8,
}

#[derive(Clone, Debug, Default)]
pub struct VideoAllocationObservation {
    /// Completed allocation passes, including passes that leave assignments unchanged.
    pub pass: u64,
    pub slots: Vec<VideoSlotObservation>,
}

#[derive(Clone, Debug)]
pub struct ShardParticipantObservation {
    pub id: ParticipantId,
    pub connection: ConnectionId,
    pub transport: NodeTransportAddress,
    pub video: VideoAllocationObservation,
}

#[derive(Clone, Debug)]
pub struct ShardObservation {
    pub shard: ShardId,
    pub generation: u64,
    pub pending_generation: Option<u64>,
    pub pending_effects: usize,
    pub participants: Vec<ShardParticipantObservation>,
    pub transports: Vec<(NodeTransportAddress, Option<ParticipantId>)>,
    pub tracks: Vec<TrackId>,
    pub plans: Vec<PlanObservation>,
    pub routes: Vec<RouteObservation>,
}

#[derive(Clone, Debug)]
pub struct Observation {
    pub control: ControlObservation,
    pub shards: Vec<ShardObservation>,
}

#[derive(Debug)]
pub struct PendingObservation {
    pub control: ControlObservation,
    pub shards: Vec<oneshot::Receiver<ShardObservation>>,
}

#[derive(Clone)]
pub struct Observer {
    commands: mailbox::Sender<ControllerCommand>,
}

impl Observer {
    pub(crate) fn new(commands: mailbox::Sender<ControllerCommand>) -> Self {
        Self { commands }
    }

    pub async fn observe(&self) -> Result<Observation, &'static str> {
        let (reply, receive) = oneshot::channel();
        self.commands
            .try_send(ControllerCommand::Observe(reply))
            .map_err(|_| "controller observation mailbox unavailable")?;
        let pending = receive
            .await
            .map_err(|_| "controller observation cancelled")?;
        let mut shards = Vec::with_capacity(pending.shards.len());
        for receive in pending.shards {
            shards.push(receive.await.map_err(|_| "shard observation cancelled")?);
        }
        Ok(Observation {
            control: pending.control,
            shards,
        })
    }
}
