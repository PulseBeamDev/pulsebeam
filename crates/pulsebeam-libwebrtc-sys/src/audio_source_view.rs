//! Owning interface views and queued native audio notifications.
use crate::{
    AudioSource, AudioSourceControlError, AudioSourceOptions, AudioSourceState, AudioTrack,
    PeerError, PeerErrorKind, ffi,
};
use std::{num::NonZeroUsize, rc::Rc};

// Native fields are destroyed first, while their origin owner still retains
// every native role thread and, for received tracks, the native peer.
#[derive(Clone)]
#[allow(dead_code)] // Variants retain origin resources until native release.
enum Origin {
    Producer(AudioSource),
    Track(AudioTrack),
}
struct ViewInner {
    native: cxx::UniquePtr<ffi::NativeAudioSourceView>,
    _origin: Origin,
}

/// An owning AudioSourceInterface view. It does not invent a caller-fed PCM
/// producer or a stop operation for remote/device sources.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::AudioSourceView>();
/// ```
#[derive(Clone)]
pub struct AudioSourceView {
    inner: Rc<ViewInner>,
}
impl AudioSourceView {
    fn new(native: cxx::UniquePtr<ffi::NativeAudioSourceView>, origin: Origin) -> Option<Self> {
        (!native.is_null()).then(|| Self {
            inner: Rc::new(ViewInner {
                native,
                _origin: origin,
            }),
        })
    }
    fn native(&self) -> &ffi::NativeAudioSourceView {
        self.inner
            .native
            .as_ref()
            .expect("owning audio source view")
    }
    pub fn same_resource(&self, other: &Self) -> bool {
        ffi::audio_source_view_same(self.native(), other.native())
    }
    pub fn state(&self) -> AudioSourceState {
        AudioSourceState::from_native(ffi::audio_source_view_state(self.native()))
    }
    pub fn is_remote(&self) -> bool {
        ffi::audio_source_view_remote(self.native())
    }
    pub fn options(&self) -> AudioSourceOptions {
        AudioSourceOptions::from_native(ffi::audio_source_view_options(self.native()))
    }
    /// Native void invocation, not a promise of effective channel processing.
    pub fn set_options(&self, options: &AudioSourceOptions) -> Result<(), AudioSourceControlError> {
        AudioSourceControlError::result(ffi::audio_source_view_set_options(
            self.native(),
            &options.ffi(),
        ))
    }
    pub fn volume(&self) -> f64 {
        ffi::audio_source_view_volume(self.native())
    }
    pub fn set_volume(&self, volume: f64) -> Result<(), AudioSourceControlError> {
        AudioSourceControlError::result(ffi::audio_source_view_set_volume(self.native(), volume))
    }
    pub fn observe_changes(&self, capacity: NonZeroUsize) -> Result<AudioObserver, PeerError> {
        self.observe(capacity, false)
    }
    /// Register the native AudioObserver. Some native sources implement this
    /// as a no-op; no synthetic volume notification is added for them.
    pub fn observe_volume(&self, capacity: NonZeroUsize) -> Result<AudioObserver, PeerError> {
        self.observe(capacity, true)
    }
    fn observe(&self, capacity: NonZeroUsize, volume: bool) -> Result<AudioObserver, PeerError> {
        AudioObserver::new(
            ffi::audio_source_view_observe(self.native(), volume, capacity.get()),
            ObserverOrigin::Source(self.clone()),
        )
    }
}
impl AudioSource {
    pub fn as_source_view(&self) -> AudioSourceView {
        AudioSourceView::new(
            ffi::audio_source_view(self.native()),
            Origin::Producer(self.clone()),
        )
        .expect("an owning producer has its native source")
    }
    pub fn observe_changes(&self, capacity: NonZeroUsize) -> Result<AudioObserver, PeerError> {
        self.as_source_view().observe_changes(capacity)
    }
}
impl AudioTrack {
    /// The actual source returned by native GetSource, with origin retention.
    pub fn source(&self) -> Option<AudioSourceView> {
        AudioSourceView::new(
            ffi::audio_track_source_view(self.native()),
            Origin::Track(self.clone()),
        )
    }
    pub fn observe_changes(&self, capacity: NonZeroUsize) -> Result<AudioObserver, PeerError> {
        AudioObserver::new(
            ffi::audio_track_observe(self.native(), capacity.get()),
            ObserverOrigin::Track(self.clone()),
        )
    }
}

/// A copied native notification. Changed has no invented state snapshot or
/// timestamp; use the owning source/track getters for current state.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AudioObserverEvent {
    Changed,
    Volume(f64),
}
#[derive(Clone)]
#[allow(dead_code)] // Variants retain origin resources until native release.
enum ObserverOrigin {
    Source(AudioSourceView),
    Track(AudioTrack),
}
struct ObserverInner {
    native: cxx::UniquePtr<ffi::NativeAudioObserver>,
    _origin: ObserverOrigin,
}
/// Bounded FIFO of actual native callbacks. Overflow drops the oldest event
/// and increments a saturating counter. Close unregisters synchronously, but
/// already queued events remain consumable. Aliases share registration/queue.
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<pulsebeam_webrtc_sys::AudioObserver>();
/// ```
#[derive(Clone)]
pub struct AudioObserver {
    inner: Rc<ObserverInner>,
}
impl AudioObserver {
    fn new(
        native: cxx::UniquePtr<ffi::NativeAudioObserver>,
        origin: ObserverOrigin,
    ) -> Result<Self, PeerError> {
        if native.is_null() {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidState,
                message: "native audio observer registration unavailable".into(),
            });
        }
        Ok(Self {
            inner: Rc::new(ObserverInner {
                native,
                _origin: origin,
            }),
        })
    }
    fn native(&self) -> &ffi::NativeAudioObserver {
        self.inner.native.as_ref().expect("owning audio observer")
    }
    pub fn try_next_event(&self) -> Option<AudioObserverEvent> {
        let event = ffi::audio_observer_take(self.native());
        event.present.then_some({
            if event.volume {
                AudioObserverEvent::Volume(event.value)
            } else {
                AudioObserverEvent::Changed
            }
        })
    }
    pub fn take_dropped_events(&self) -> u64 {
        ffi::audio_observer_take_dropped(self.native())
    }
    pub fn close(&self) {
        ffi::audio_observer_close(self.native());
    }
}
