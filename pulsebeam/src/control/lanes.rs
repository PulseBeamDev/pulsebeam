//! Per-lane data stream routing state.
//!
//! The two lanes are the same data channel. Reliable delivery is unreliable
//! delivery plus a retransmit cache and the feedback to drive it, layered
//! end-to-end over a hop-to-hop transport that guarantees neither — so the
//! routing state machine is identical and only the runtime key differs.
//! Keeping them as one type instantiated twice is what stops the two copies
//! drifting: every lane-dependent decision — which key to mint, which route
//! action to emit, which arena to retire into — is a method here, and nowhere
//! else.

use crate::{
    control::state::{ControlModel, DataStreamId},
    id::ShardId,
    route::RouteAction,
};

/// How a data stream is delivered.
///
/// Named for the guarantee rather than the medium: both lanes carry the same
/// data channel, and `Unreliable` is the base the other adds to. The lane is
/// part of a stream's identity, not a flag on it — `Topic::publisher()`
/// resolves to `.ordered()` or `.latest()`, and one topic name can carry both
/// at once without either claiming it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum StreamLane {
    Unreliable,
    Reliable,
}

impl From<StreamLane> for crate::track::DataLane {
    fn from(lane: StreamLane) -> Self {
        match lane {
            StreamLane::Unreliable => Self::Realtime,
            StreamLane::Reliable => Self::Reliable,
        }
    }
}

impl From<crate::track::DataLane> for StreamLane {
    fn from(lane: crate::track::DataLane) -> Self {
        match lane {
            crate::track::DataLane::Realtime => Self::Unreliable,
            crate::track::DataLane::Reliable => Self::Reliable,
        }
    }
}

pub(crate) struct LaneRegistry {
    lane: StreamLane,
}

impl LaneRegistry {
    pub(crate) fn new(lane: StreamLane) -> Self {
        Self { lane }
    }

    /// The route action that carries this lane's traffic. `None` when the key
    /// belongs to the other lane, which is a caller bug rather than a state.
    pub(crate) fn route_action(&self, key: crate::keys::TrackKey) -> RouteAction {
        RouteAction::Forward { target: key }
    }

    pub(crate) fn mint(
        &self,
        state: &mut ControlModel,
        destination: ShardId,
        id: &DataStreamId,
    ) -> Option<crate::keys::TrackKey> {
        let track_id = id.publisher_id.derive_track_id(
            crate::entity::TrackKind::Data,
            &crate::track::publication_label(self.lane.into(), &id.topic),
        );
        state.mint_track(destination, track_id, id.publisher_id)
    }
}

/// Both lanes, so a caller that must touch each does not name them separately.
pub(crate) struct Lanes {
    data: LaneRegistry,
    reliable: LaneRegistry,
}

impl Lanes {
    pub(crate) fn new() -> Self {
        Self {
            data: LaneRegistry::new(StreamLane::Unreliable),
            reliable: LaneRegistry::new(StreamLane::Reliable),
        }
    }

    pub(crate) fn get(&self, lane: StreamLane) -> &LaneRegistry {
        match lane {
            StreamLane::Unreliable => &self.data,
            StreamLane::Reliable => &self.reliable,
        }
    }
}

#[cfg(test)]
mod tests;
