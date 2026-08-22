#![forbid(unsafe_code)]

pub mod core;
pub mod time;
pub mod types;

#[cfg(test)]
pub mod test_utils;

pub use crate::core::{AgentCore, CoreEffect, CoreError, CoreEvent, CoreInput};
pub use crate::time::MonotonicTime;
pub use crate::types::{
    ChannelKey, ConnectionState, CoreConfig, MediaKind, MediaSlotId, ParticipantId,
    ReconnectPolicy, RequestId, TrackId, TransportGeneration,
};
