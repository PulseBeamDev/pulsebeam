use crate::entity::TrackId;

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
