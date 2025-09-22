use std::collections::HashMap;

use str0m::media::Mid;

use crate::{
    participant::{audio::AudioAllocator, video::VideoAllocator},
    track,
};

pub struct ParticipantCore {
    published_tracks: HashMap<Mid, track::TrackHandle>,
    video_allocator: VideoAllocator,
    audio_allocator: AudioAllocator,
}

impl ParticipantCore {
    pub fn new() -> Self {
        Self {
            published_tracks: HashMap::new(),
            video_allocator: VideoAllocator::default(),
            audio_allocator: AudioAllocator::with_chromium_limit(),
        }
    }
}
