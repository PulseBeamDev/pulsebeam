//! Owning common track references for endpoint reads and native selectors.
use crate::{AudioTrack, VideoTrack, ffi, peer::FactoryInner};
use std::rc::Rc;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MediaTrackKind {
    Audio,
    Video,
    Other(Vec<u8>),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaTrackState {
    Live,
    Ended,
}

// Source wrappers can carry close-on-drop behavior in addition to factory
// roles. Keep local tracks intact, but never retain a remote PeerInner here:
// a pending request may select its own peer's track or another selecting peer.
pub(crate) enum MediaTrackOwner {
    Video { _track: VideoTrack },
    Audio { _track: AudioTrack },
    Factory { _factory: Rc<FactoryInner> },
}
struct TrackInner {
    native: cxx::UniquePtr<ffi::NativeMediaTrack>,
    _owner: MediaTrackOwner,
}
/// A sequence-bound SDK track reference, not a textual track-ID lookup.
#[derive(Clone)]
pub struct MediaTrack(Rc<TrackInner>);
impl MediaTrack {
    pub(crate) fn from_native(
        native: cxx::UniquePtr<ffi::NativeMediaTrack>,
        owner: MediaTrackOwner,
    ) -> Self {
        assert!(!native.is_null());
        Self(Rc::new(TrackInner {
            native,
            _owner: owner,
        }))
    }
    pub(crate) fn factory(&self) -> &Rc<FactoryInner> {
        match &self.0._owner {
            MediaTrackOwner::Audio { _track } => _track.factory_context(),
            MediaTrackOwner::Video { _track } => _track.factory_context(),
            MediaTrackOwner::Factory { _factory } => _factory,
        }
    }
    pub(crate) fn source_owner(&self) -> MediaTrackOwner {
        match &self.0._owner {
            MediaTrackOwner::Video { _track } => _track.media_track_owner(),
            MediaTrackOwner::Audio { _track } => _track.media_track_owner(),
            MediaTrackOwner::Factory { _factory } => MediaTrackOwner::Factory {
                _factory: _factory.clone(),
            },
        }
    }
    pub(crate) fn local_video(&self) -> Option<&VideoTrack> {
        match &self.0._owner {
            MediaTrackOwner::Video { _track } => Some(_track),
            _ => None,
        }
    }
    pub(crate) fn local_audio(&self) -> Option<&AudioTrack> {
        match &self.0._owner {
            MediaTrackOwner::Audio { _track } => Some(_track),
            _ => None,
        }
    }
    pub(crate) fn native(&self) -> &ffi::NativeMediaTrack {
        self.0.native.as_ref().unwrap()
    }
    pub fn same_resource(&self, other: &Self) -> bool {
        ffi::media_track_same_resource(self.native(), other.native())
    }
    pub fn id(&self) -> Vec<u8> {
        ffi::media_track_id(self.native())
    }
    pub fn kind(&self) -> MediaTrackKind {
        match ffi::media_track_kind(self.native()).as_slice() {
            b"audio" => MediaTrackKind::Audio,
            b"video" => MediaTrackKind::Video,
            other => MediaTrackKind::Other(other.to_vec()),
        }
    }
    pub fn state(&self) -> MediaTrackState {
        match ffi::media_track_state(self.native()) {
            0 => MediaTrackState::Live,
            1 => MediaTrackState::Ended,
            _ => unreachable!("pinned media track state"),
        }
    }
    pub fn enabled(&self) -> bool {
        ffi::media_track_enabled(self.native())
    }
    pub fn set_enabled(&self, enabled: bool) -> bool {
        ffi::media_track_set_enabled(self.native(), enabled)
    }
}
