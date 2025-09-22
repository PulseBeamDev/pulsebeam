use crate::track;

pub enum Effect {
    Subscribe(track::TrackHandle),
}
