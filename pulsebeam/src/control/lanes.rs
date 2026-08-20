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
    control::state::ControlPlaneState,
    id::ShardId,
    route::RouteAction,
    shard::router::{DataStreamId, RuntimeStreamKey},
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

impl StreamLane {
    /// The lane a runtime key belongs to. The key's variant is the only thing
    /// that decides it, so this is the one place that mapping is written.
    pub(crate) fn of(key: RuntimeStreamKey) -> Self {
        match key {
            RuntimeStreamKey::Unreliable(_) => StreamLane::Unreliable,
            RuntimeStreamKey::Reliable(_) => StreamLane::Reliable,
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
    pub(crate) fn route_action(&self, key: RuntimeStreamKey) -> Option<RouteAction> {
        match (self.lane, key) {
            (StreamLane::Unreliable, RuntimeStreamKey::Unreliable(stream)) => {
                Some(RouteAction::Unreliable { stream })
            }
            (StreamLane::Reliable, RuntimeStreamKey::Reliable(stream)) => {
                Some(RouteAction::Reliable { stream })
            }
            _ => None,
        }
    }

    pub(crate) fn mint(
        &self,
        state: &mut ControlPlaneState,
        destination: ShardId,
        id: &DataStreamId,
    ) -> Option<RuntimeStreamKey> {
        match self.lane {
            StreamLane::Unreliable => state
                .mint_data(destination, id.clone())
                .map(RuntimeStreamKey::Unreliable),
            StreamLane::Reliable => state
                .mint_reliable(destination, id.clone())
                .map(RuntimeStreamKey::Reliable),
        }
    }

    pub(crate) fn retire_runtime(
        &self,
        state: &mut ControlPlaneState,
        destination: ShardId,
        key: RuntimeStreamKey,
    ) {
        match (self.lane, key) {
            (StreamLane::Unreliable, RuntimeStreamKey::Unreliable(key)) => {
                state.remove_data(destination, key);
            }
            (StreamLane::Reliable, RuntimeStreamKey::Reliable(key)) => {
                state.remove_reliable(destination, key);
            }
            _ => debug_assert!(false, "stream key and lane disagree"),
        }
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
