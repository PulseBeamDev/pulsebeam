use std::{collections::VecDeque, sync::Arc};

use crate::{message::TrackMeta, track};

pub type Queue = VecDeque<Effect>;

pub enum Effect {
    Subscribe(track::TrackHandle),
    SpawnTrack(Arc<TrackMeta>),
    Disconnect,
}
