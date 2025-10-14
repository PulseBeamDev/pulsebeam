use std::{collections::VecDeque, sync::Arc};

use crate::{entity::TrackId, track::TrackMeta};

pub type Queue = VecDeque<Effect>;

#[derive(Debug)]
pub enum Effect {
    Subscribe(Arc<TrackId>),
    SpawnTrack(Arc<TrackMeta>),
    Disconnect,
}
