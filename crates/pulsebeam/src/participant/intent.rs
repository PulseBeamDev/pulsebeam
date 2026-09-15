use crate::entity::TrackId;
use crate::entity::TrackKind;

/// Normalized native send entry. The decoder keeps malformed coordinates as
/// `None` so publication reconciliation can skip them without changing the
/// accepted replacement state.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "the replacement signaling transaction consumes normalized publications in Plan 07"
)]
pub(crate) struct NativePublication {
    pub(crate) sender_index: Option<u32>,
    pub(crate) kind: Option<TrackKind>,
    pub(crate) label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioIntent {
    pub pinned: Vec<TrackId>,
    pub auto: bool,
}

impl Default for AudioIntent {
    fn default() -> Self {
        Self {
            pinned: Vec::new(),
            auto: true,
        }
    }
}

#[derive(Clone)]
pub struct VideoIntent {
    pub track_id: TrackId,
    pub target_height: u32,
    pub min_height: u32,
    pub min_fps: u32,
    pub priority: u32,
}
