use crate::{AudioTrack, PeerError, PeerErrorKind, VideoTrack, ffi, peer::FactoryInner};
use std::{cell::RefCell, num::NonZeroUsize, rc::Rc};

/// An owning native track container. Membership does not synchronize received
/// media or change SDP stream routing. Native Add rejects duplicate IDs per
/// kind; native Remove matches the supplied track's ID, not its identity.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::LocalMediaStream>();
/// ```
#[derive(Clone)]
pub struct LocalMediaStream {
    inner: Rc<StreamInner>,
}
struct StreamInner {
    native: cxx::UniquePtr<ffi::NativeLocalMediaStream>,
    // Exact successful Add arguments retain their actual source/peer/factory
    // dependencies. These are ownership edges, not a cached membership result.
    // Track wrappers never point back to the stream, so the graph is acyclic.
    audio_dependencies: RefCell<Vec<AudioTrack>>,
    video_dependencies: RefCell<Vec<VideoTrack>>,
    _factory: Rc<FactoryInner>,
}
impl LocalMediaStream {
    pub(crate) fn from_native(
        native: cxx::UniquePtr<ffi::NativeLocalMediaStream>,
        factory: Rc<FactoryInner>,
    ) -> Self {
        Self {
            inner: Rc::new(StreamInner {
                native,
                audio_dependencies: RefCell::new(Vec::new()),
                video_dependencies: RefCell::new(Vec::new()),
                _factory: factory,
            }),
        }
    }
    pub fn id(&self) -> String {
        ffi::local_media_stream_id(self.native())
    }
    pub fn audio_tracks(&self) -> Vec<AudioTrack> {
        ffi::local_media_stream_track_keys(self.native(), true)
            .into_iter()
            .map(|key| self.resolve_audio(key))
            .collect()
    }
    pub fn video_tracks(&self) -> Vec<VideoTrack> {
        ffi::local_media_stream_track_keys(self.native(), false)
            .into_iter()
            .map(|key| self.resolve_video(key))
            .collect()
    }
    pub fn find_audio_track(&self, id: &str) -> Option<AudioTrack> {
        let key = ffi::local_media_stream_find_track(self.native(), true, id);
        (key != 0).then(|| self.resolve_audio(key))
    }
    pub fn find_video_track(&self, id: &str) -> Option<VideoTrack> {
        let key = ffi::local_media_stream_find_track(self.native(), false, id);
        (key != 0).then(|| self.resolve_video(key))
    }
    pub fn add_audio_track(&self, track: &AudioTrack) -> bool {
        if !ffi::local_media_stream_audio_track(self.native(), track.native(), true) {
            return false;
        }
        self.inner
            .audio_dependencies
            .borrow_mut()
            .push(track.clone());
        true
    }
    pub fn add_video_track(&self, track: &VideoTrack) -> bool {
        if !ffi::local_media_stream_video_track(self.native(), track.native(), true) {
            return false;
        }
        self.inner
            .video_dependencies
            .borrow_mut()
            .push(track.clone());
        true
    }
    pub fn remove_audio_track(&self, track: &AudioTrack) -> bool {
        if !ffi::local_media_stream_audio_track(self.native(), track.native(), false) {
            return false;
        }
        let actual = ffi::local_media_stream_track_keys(self.native(), true);
        self.inner
            .audio_dependencies
            .borrow_mut()
            .retain(|owner| actual.contains(&ffi::audio_track_identity(owner.native())));
        true
    }
    pub fn remove_video_track(&self, track: &VideoTrack) -> bool {
        if !ffi::local_media_stream_video_track(self.native(), track.native(), false) {
            return false;
        }
        let actual = ffi::local_media_stream_track_keys(self.native(), false);
        self.inner
            .video_dependencies
            .borrow_mut()
            .retain(|owner| actual.contains(&ffi::video_track_identity(owner.native())));
        true
    }
    pub fn observe_changes(
        &self,
        capacity: NonZeroUsize,
    ) -> Result<MediaStreamObserver, PeerError> {
        let native = ffi::local_media_stream_observe(self.native(), capacity.get());
        if native.is_null() {
            return Err(PeerError {
                kind: PeerErrorKind::NativeConstruction,
                message: "native stream observer creation failed".into(),
            });
        }
        Ok(MediaStreamObserver {
            inner: Rc::new(ObserverInner {
                native,
                _stream: self.clone(),
            }),
        })
    }
    fn resolve_audio(&self, key: usize) -> AudioTrack {
        // Get/Find returned a real native identity. Resolve only against the
        // exact accepted track owners, never by user-visible IDs or heuristics.
        self.inner
            .audio_dependencies
            .borrow()
            .iter()
            .find(|owner| ffi::audio_track_identity(owner.native()) == key)
            .expect("native stream track has its retained Add dependency")
            .clone()
    }
    fn resolve_video(&self, key: usize) -> VideoTrack {
        self.inner
            .video_dependencies
            .borrow()
            .iter()
            .find(|owner| ffi::video_track_identity(owner.native()) == key)
            .expect("native stream track has its retained Add dependency")
            .clone()
    }
    fn native(&self) -> &ffi::NativeLocalMediaStream {
        self.inner.native.as_ref().expect("owning native stream")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaStreamEvent {
    Changed,
}
/// Bounded native change notifications. Closing synchronously unregisters;
/// accepted events and overflow counts remain consumable after close.
#[derive(Clone)]
pub struct MediaStreamObserver {
    inner: Rc<ObserverInner>,
}
struct ObserverInner {
    native: cxx::UniquePtr<ffi::NativeLocalMediaStreamObserver>,
    _stream: LocalMediaStream,
}
impl MediaStreamObserver {
    pub fn try_next_event(&self) -> Option<MediaStreamEvent> {
        ffi::local_media_stream_observer_take(self.native()).then_some(MediaStreamEvent::Changed)
    }
    pub fn take_dropped_events(&self) -> u64 {
        ffi::local_media_stream_observer_take_dropped(self.native())
    }
    pub fn close(&self) {
        ffi::local_media_stream_observer_close(self.native());
    }
    fn native(&self) -> &ffi::NativeLocalMediaStreamObserver {
        self.inner.native.as_ref().expect("owning stream observer")
    }
}
