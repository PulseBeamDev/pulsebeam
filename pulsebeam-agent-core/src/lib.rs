#![forbid(unsafe_code)]

pub mod core;
pub mod e2ee;
pub mod http;
pub mod intent;
pub mod lifecycle;
pub mod preset;
pub mod session;
pub mod time;
pub mod topic;
pub mod types;

#[cfg(test)]
pub mod test_utils;

pub use crate::core::{AgentCore, CoreEffect, CoreError, CoreEvent, CoreInput};
pub use crate::e2ee::{E2EE_FRAME_VERSION, E2eeError, E2eeFrame, E2eeKey, E2eeSession};
pub use crate::http::{HttpHeader, HttpMethod, HttpRequest, HttpResponse, HttpStatusError};
pub use crate::intent::{
    AudioIntent, IntentError, IntentState, LayerOption, StickyAllocation, StickyAllocator,
    VideoIntent,
};
pub use crate::lifecycle::{
    Lifecycle, LifecycleEffect, LifecycleError, LifecycleEvent, LifecycleInput, LifecycleState,
};
pub use crate::preset::{LatencyLock, LatencyLockError, PlayoutPreset, VideoPreset};
pub use crate::session::{
    AudioBindingState, PublicationState, SessionError, SessionEvent, SessionReducer,
    SessionSnapshot, VideoBindingState,
};
pub use crate::time::MonotonicTime;
pub use crate::topic::{
    LatestMessage, LatestTopic, OrderedEvent, OrderedReceiver, TopicPublisher, TopicStream,
};
pub use crate::types::{
    ChannelKey, ConnectionState, CoreConfig, MediaKind, MediaSlotId, ParticipantId,
    ReconnectPolicy, RequestId, TrackId, TransportGeneration,
};
