slotmap::new_key_type! {
    pub struct ParticipantKey;
    pub struct TrackKey;
    pub struct UnreliableStreamKey;
    pub struct ReliableStreamKey;
    pub struct DownstreamSlotKey;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct VideoTrackKey(TrackKey);

impl VideoTrackKey {
    pub(crate) const fn new(key: TrackKey) -> Self {
        Self(key)
    }

    pub(crate) const fn raw(self) -> TrackKey {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct AudioTrackKey(TrackKey);

impl AudioTrackKey {
    pub(crate) const fn new(key: TrackKey) -> Self {
        Self(key)
    }

    pub(crate) const fn raw(self) -> TrackKey {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrackRuntimeKey {
    Video(VideoTrackKey),
    Audio(AudioTrackKey),
}

impl TrackRuntimeKey {
    pub(crate) const fn raw(self) -> TrackKey {
        match self {
            Self::Video(key) => key.raw(),
            Self::Audio(key) => key.raw(),
        }
    }
}
