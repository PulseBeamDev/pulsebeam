use std::{collections::VecDeque, sync::Arc};

use crate::{message::TrackMeta, track};

pub type Queue = VecDeque<Effect>;

#[derive(Debug)]
pub enum Effect {
    Subscribe(track::TrackReceiver),
    SpawnTrack(Arc<TrackMeta>),
    Disconnect,
}
