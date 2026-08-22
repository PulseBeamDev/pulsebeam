#![forbid(unsafe_code)]

pub mod core;
pub mod http;
pub mod preset;
pub mod time;
pub mod types;

#[cfg(feature = "e2ee")]
pub mod e2ee;
#[cfg(feature = "protocol")]
pub mod intent;
#[cfg(feature = "protocol")]
pub mod session;
#[cfg(feature = "protocol")]
pub mod topic;

#[cfg(test)]
pub mod test_utils;

pub use crate::core::{AgentCore, CoreEffect, CoreError, CoreEvent, CoreInput};
#[cfg(feature = "e2ee")]
pub use crate::e2ee::{
    E2EE_FRAME_VERSION, E2eeDirection, E2eeDomain, E2eeEncryptor, E2eeEpoch, E2eeError, E2eeFrame,
    E2eeKeyRing, E2eeMasterKey, E2eeReceiver,
};
pub use crate::http::{HttpHeader, HttpMethod, HttpRequest, HttpResponse, HttpStatusError};
#[cfg(feature = "protocol")]
pub use crate::intent::{
    AudioIntent, IntentError, IntentState, LayerOption, StickyAllocation, StickyAllocator,
    VideoIntent,
};
pub use crate::preset::{LatencyLock, LatencyLockError, PlayoutPreset, VideoPreset};
#[cfg(feature = "protocol")]
pub use crate::session::{
    AudioBindingState, PublicationState, SessionError, SessionEvent, SessionReducer,
    SessionSnapshot, VideoBindingState,
};
pub use crate::time::MonotonicTime;
#[cfg(feature = "protocol")]
pub use crate::topic::{
    LatestMessage, LatestTopic, OrderedEvent, OrderedReceiver, TopicPublisher, TopicStream,
};
pub use crate::types::{
    ChannelKey, ConnectionState, CoreConfig, MediaKind, MediaSlotId, ParticipantId,
    ReconnectPolicy, RequestId, TrackId, TransportGeneration,
};
