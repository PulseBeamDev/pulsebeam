//! Owning built-in track sources, receiver streams and explicit SDK notifiers.
use crate::{AudioSourceOptions, VideoContentHint, VideoSourceStats};
use crate::{MediaTrack, MediaTrackKind, ffi, media_track::MediaTrackOwner, peer::FactoryInner};
use std::{num::NonZeroUsize, rc::Rc};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaSourceState {
    Initializing,
    Live,
    Ended,
    Muted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoSourceInfo {
    pub is_screencast: bool,
    pub needs_denoising: Option<bool>,
    pub supports_encoded_output: bool,
    pub stats: Option<VideoSourceStats>,
}
struct SourceInner {
    native: cxx::UniquePtr<ffi::NativeTrackSource>,
    _track: MediaTrack,
}
#[derive(Clone)]
pub struct AudioTrackSource(Rc<SourceInner>);
#[derive(Clone)]
pub struct VideoTrackSource(Rc<SourceInner>);
#[derive(Clone)]
pub enum MediaTrackSource {
    Audio(AudioTrackSource),
    Video(VideoTrackSource),
}
macro_rules! source_reads {
    ($ty:ident) => {
        impl $ty {
            pub(crate) fn native(&self) -> &ffi::NativeTrackSource {
                self.0.native.as_ref().unwrap()
            }
            pub fn state(&self) -> MediaSourceState {
                match ffi::track_source_state(self.native()) {
                    0 => MediaSourceState::Initializing,
                    1 => MediaSourceState::Live,
                    2 => MediaSourceState::Ended,
                    3 => MediaSourceState::Muted,
                    _ => unreachable!("pinned source state"),
                }
            }
            pub fn remote(&self) -> bool {
                ffi::track_source_remote(self.native())
            }
            pub fn same_resource(&self, other: &Self) -> bool {
                ffi::track_source_same_resource(self.native(), other.native())
            }
        }
    };
}
source_reads!(AudioTrackSource);
source_reads!(VideoTrackSource);
impl AudioTrackSource {
    pub fn options(&self) -> AudioSourceOptions {
        AudioSourceOptions::from_native(ffi::track_source_audio_options(self.native()))
    }
}
impl VideoTrackSource {
    pub fn info(&self) -> VideoSourceInfo {
        let x = ffi::track_source_video_info(self.native());
        VideoSourceInfo {
            is_screencast: x.is_screencast,
            needs_denoising: boolean(x.needs_denoising),
            supports_encoded_output: x.supports_encoded_output,
            stats: x.has_stats.then_some(VideoSourceStats {
                input_width: x.input_width,
                input_height: x.input_height,
            }),
        }
    }
}
impl MediaTrackSource {
    pub fn kind(&self) -> MediaTrackKind {
        match self {
            Self::Audio(_) => MediaTrackKind::Audio,
            Self::Video(_) => MediaTrackKind::Video,
        }
    }
    pub fn state(&self) -> MediaSourceState {
        match self {
            Self::Audio(x) => x.state(),
            Self::Video(x) => x.state(),
        }
    }
    pub fn remote(&self) -> bool {
        match self {
            Self::Audio(x) => x.remote(),
            Self::Video(x) => x.remote(),
        }
    }
    pub fn same_resource(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Audio(a), Self::Audio(b)) => a.same_resource(b),
            (Self::Video(a), Self::Video(b)) => a.same_resource(b),
            _ => false,
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct AudioProcessorStats {
    pub typing_noise_detected: bool,
    pub voice_detected: Option<bool>,
    pub echo_return_loss: Option<f64>,
    pub echo_return_loss_enhancement: Option<f64>,
    pub divergent_filter_fraction: Option<f64>,
    pub residual_echo_likelihood: Option<f64>,
    pub residual_echo_likelihood_recent_max: Option<f64>,
    pub delay_median_ms: Option<i32>,
    pub delay_standard_deviation_ms: Option<i32>,
    pub delay_ms: Option<i32>,
}
struct ProcessorInner {
    native: cxx::UniquePtr<ffi::NativeAudioProcessor>,
    _track: MediaTrack,
}
#[derive(Clone)]
pub struct AudioProcessor(Rc<ProcessorInner>);
impl AudioProcessor {
    fn native(&self) -> &ffi::NativeAudioProcessor {
        self.0.native.as_ref().unwrap()
    }
    pub fn same_resource(&self, other: &Self) -> bool {
        ffi::audio_processor_same_resource(self.native(), other.native())
    }
    pub fn stats(&self, has_remote_tracks: bool) -> AudioProcessorStats {
        let x = ffi::audio_processor_stats(self.native(), has_remote_tracks);
        AudioProcessorStats {
            typing_noise_detected: x.typing_noise_detected,
            voice_detected: boolean(x.voice_detected),
            echo_return_loss: x
                .echo_return_loss
                .present
                .then_some(x.echo_return_loss.value),
            echo_return_loss_enhancement: x
                .echo_return_loss_enhancement
                .present
                .then_some(x.echo_return_loss_enhancement.value),
            divergent_filter_fraction: x
                .divergent_filter_fraction
                .present
                .then_some(x.divergent_filter_fraction.value),
            residual_echo_likelihood: x
                .residual_echo_likelihood
                .present
                .then_some(x.residual_echo_likelihood.value),
            residual_echo_likelihood_recent_max: x
                .residual_echo_likelihood_recent_max
                .present
                .then_some(x.residual_echo_likelihood_recent_max.value),
            delay_median_ms: x.delay_median_ms.present.then_some(x.delay_median_ms.value),
            delay_standard_deviation_ms: x
                .delay_standard_deviation_ms
                .present
                .then_some(x.delay_standard_deviation_ms.value),
            delay_ms: x.delay_ms.present.then_some(x.delay_ms.value),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrackAudioOptionsCode {
    Applied,
    Stored,
    RejectedRemoteTrack,
    RejectedInvalidCombination,
    RejectedPlatformUnavailable,
    ApplyFailed,
    Other(i32),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackAudioOptionsResult {
    pub code: TrackAudioOptionsCode,
    pub message: Vec<u8>,
}
impl TrackAudioOptionsResult {
    pub fn ok(&self) -> bool {
        matches!(
            self.code,
            TrackAudioOptionsCode::Applied | TrackAudioOptionsCode::Stored
        )
    }
}

fn boolean(x: i8) -> Option<bool> {
    match x {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}
impl MediaTrack {
    fn require_kind(&self, kind: MediaTrackKind) -> Result<(), crate::PeerError> {
        if self.kind() == kind {
            Ok(())
        } else {
            Err(crate::PeerError {
                kind: crate::PeerErrorKind::InvalidParameter,
                message: "operation requires the corresponding audio/video track kind".into(),
            })
        }
    }
    /// Forward the native void property update. No source/encoder state is fabricated.
    pub fn set_should_receive(&self, receive: bool) -> Result<(), crate::PeerError> {
        self.require_kind(MediaTrackKind::Video)?;
        ffi::media_track_set_should_receive(self.native(), receive);
        Ok(())
    }
    pub fn set_content_hint(&self, hint: VideoContentHint) -> Result<(), crate::PeerError> {
        self.require_kind(MediaTrackKind::Video)?;
        ffi::media_track_set_content_hint(self.native(), hint as i32);
        Ok(())
    }
    /// SDK volume domain is [0,10]. Some local sources inherit the SDK no-op;
    /// read back the actual value instead of treating this void call as an echo.
    pub fn set_audio_volume(&self, volume: f64) -> Result<(), crate::PeerError> {
        self.require_kind(MediaTrackKind::Audio)?;
        if !volume.is_finite() || !(0.0..=10.0).contains(&volume) {
            return Err(crate::PeerError {
                kind: crate::PeerErrorKind::InvalidParameter,
                message: "audio volume must be finite and in [0,10]".into(),
            });
        }
        ffi::media_track_set_volume(self.native(), volume);
        Ok(())
    }
    /// Preserve every option and the actual SDK result code/message. Native
    /// AudioTrack stores its supported processing subset, not unrelated jitter
    /// or network options from this complete value.
    pub fn set_audio_options(
        &self,
        options: &AudioSourceOptions,
    ) -> Result<TrackAudioOptionsResult, crate::PeerError> {
        self.require_kind(MediaTrackKind::Audio)?;
        let x = ffi::media_track_set_audio_options(self.native(), &options.ffi());
        Ok(TrackAudioOptionsResult {
            code: match x.code {
                0 => TrackAudioOptionsCode::Applied,
                1 => TrackAudioOptionsCode::Stored,
                2 => TrackAudioOptionsCode::RejectedRemoteTrack,
                3 => TrackAudioOptionsCode::RejectedInvalidCombination,
                4 => TrackAudioOptionsCode::RejectedPlatformUnavailable,
                5 => TrackAudioOptionsCode::ApplyFailed,
                other => TrackAudioOptionsCode::Other(other),
            },
            message: x.message,
        })
    }
    pub fn source(&self) -> Option<MediaTrackSource> {
        let video = match self.kind() {
            MediaTrackKind::Audio => false,
            MediaTrackKind::Video => true,
            _ => return None,
        };
        let native = ffi::media_track_source(self.native(), self.factory().native(), video);
        if native.is_null() {
            return None;
        }
        let inner = Rc::new(SourceInner {
            native,
            _track: self.clone(),
        });
        Some(if video {
            MediaTrackSource::Video(VideoTrackSource(inner))
        } else {
            MediaTrackSource::Audio(AudioTrackSource(inner))
        })
    }
    pub fn audio_volume(&self) -> Option<f64> {
        let x = ffi::media_track_audio_volume(self.native());
        x.present.then_some(x.value)
    }
    pub fn signal_level(&self) -> Option<i32> {
        let x = ffi::media_track_signal_level(self.native());
        x.present.then_some(x.value)
    }
    pub fn audio_processor(&self) -> Option<AudioProcessor> {
        let native = ffi::media_track_audio_processor(self.native(), self.factory().native());
        (!native.is_null()).then(|| {
            AudioProcessor(Rc::new(ProcessorInner {
                native,
                _track: self.clone(),
            }))
        })
    }
    pub fn should_receive(&self) -> Option<bool> {
        boolean(ffi::media_track_should_receive(self.native()) as i8)
    }
    pub fn content_hint(&self) -> Option<VideoContentHint> {
        match ffi::media_track_content_hint(self.native()) {
            -1 => None,
            0 => Some(VideoContentHint::None),
            1 => Some(VideoContentHint::Fluid),
            2 => Some(VideoContentHint::Detailed),
            3 => Some(VideoContentHint::Text),
            _ => unreachable!("pinned content hint"),
        }
    }
    /// Each call explicitly registers an independent SDK notifier. Getting or
    /// cloning a track alias never installs an observer as a side effect.
    pub fn observe(&self, capacity: NonZeroUsize) -> MediaObserver {
        MediaObserver {
            native: ffi::media_track_observe(
                self.native(),
                self.factory().native(),
                capacity.get(),
            ),
            _owner: ObserverOwner::Track {
                _track: self.clone(),
            },
        }
    }
}
#[cfg(test)]
mod stream_lease_tests {
    use super::*;
    #[test]
    fn generic_and_typed_audio_senders_share_source_leases_without_peer_cycles() {
        let factory = crate::PeerConnectionFactory::builder()
            .audio_encoder_factory(crate::AudioEncoderFactory::builtin().unwrap())
            .audio_decoder_factory(crate::AudioDecoderFactory::builtin_opus().unwrap())
            .build()
            .unwrap();
        let mut peer = factory.create_peer_connection(Default::default()).unwrap();
        let source = factory.create_audio_source().unwrap();
        let track = factory.create_audio_track("audio-source", &source).unwrap();
        let tx = peer
            .add_audio_transceiver(&track, crate::RtpTransceiverDirection::SendOnly)
            .unwrap();
        let sender = tx.sender();
        assert!(
            sender
                .set_media_track(Some(&track.as_media_track()))
                .unwrap()
        );
        let alias = sender.audio_track().unwrap();
        assert!(
            alias
                .as_media_track()
                .same_resource(&track.as_media_track())
        );
        // A generic sender's known local source is not mistaken for a received
        // alias of this same peer, which would reject this idempotent operation.
        sender.set_audio_track(Some(&alias)).unwrap();
        let replacement_source = factory.create_audio_source().unwrap();
        let replacement = factory
            .create_audio_track("replacement", &replacement_source)
            .unwrap();
        sender.set_audio_track(Some(&replacement)).unwrap();
        assert!(
            !peer
                .inner
                .stream_inputs
                .borrow()
                .contains_key(&sender.resource_key())
        );
        let received = crate::AudioTrack::received(
            ffi::rtp_sender_audio_track(peer.inner.native(), sender.native()),
            peer.inner.clone(),
        );
        assert!(received.as_media_track().local_audio().is_none());
        peer.close().unwrap();
    }
    #[test]
    fn generic_sender_video_alias_retains_original_source_without_a_peer_cycle() {
        let factory = crate::PeerConnectionFactory::builder().build().unwrap();
        let mut peer = factory.create_peer_connection(Default::default()).unwrap();
        let old_source = factory.create_video_source().unwrap();
        let old = factory.create_video_track("original", &old_source).unwrap();
        let tx = peer
            .add_video_transceiver(&old, crate::RtpTransceiverDirection::SendOnly)
            .unwrap();
        let sender = tx.sender();
        let source = factory.create_video_source().unwrap();
        let track = factory.create_video_track("replacement", &source).unwrap();
        assert!(
            sender
                .set_media_track(Some(&track.as_media_track()))
                .unwrap()
        );
        let alias = sender.track().unwrap().as_media_track();
        assert!(alias.same_resource(&track.as_media_track()));
        let stream = factory
            .create_local_media_stream(b"alias-source-owner")
            .unwrap();
        assert!(stream.add_track(&alias).unwrap());
        let source_observation = alias.source().unwrap();
        // Keep a pure SDK source facade for the retirement assertion. The
        // public source observation intentionally also owns its input track.
        let native_source = ffi::media_track_source(alias.native(), alias.factory().native(), true);
        drop(alias);
        drop(track);
        drop(source);
        peer.close().unwrap();
        drop(sender);
        drop(tx);
        drop(peer);
        drop(old);
        drop(old_source);
        assert_eq!(source_observation.state(), MediaSourceState::Live);
        drop(source_observation);
        assert_eq!(ffi::track_source_state(native_source.as_ref().unwrap()), 1);
        let retained = stream.video_tracks().pop().unwrap();
        assert!(stream.remove_track(&retained));
        drop(retained);
        assert_eq!(ffi::track_source_state(native_source.as_ref().unwrap()), 2);
    }
    #[test]
    fn retired_consumers_and_id_removed_track_leases_do_not_accumulate() {
        let factory = crate::PeerConnectionFactory::builder().build().unwrap();
        let stream = factory.create_local_media_stream(b"reused-stream").unwrap();
        for _ in 0..4 {
            let mut config = crate::RtcConfiguration::default();
            config.options.sdp_semantics = crate::SdpSemantics::PlanB;
            let peer = factory
                .create_peer_connection_with_configuration(config)
                .unwrap();
            assert!(peer.add_stream(&stream).unwrap());
            assert_eq!(stream.0.peers.borrow().len(), 1);
        }
        let source = factory.create_audio_source().unwrap();
        let first = factory
            .create_audio_track("same-id", &source)
            .unwrap()
            .as_media_track();
        let second = factory
            .create_audio_track("same-id", &source)
            .unwrap()
            .as_media_track();
        assert!(!first.same_resource(&second));
        for _ in 0..4 {
            assert!(stream.add_track(&first).unwrap());
            assert!(stream.remove_track(&second));
            assert!(stream.0.tracks.borrow().is_empty());
        }
    }
}
struct StreamInner {
    native: cxx::UniquePtr<ffi::NativeMediaStream>,
    factory: Rc<FactoryInner>,
    // Original source owners, not parent-stream edges that can form Rc cycles.
    tracks: std::cell::RefCell<Vec<MediaTrack>>,
    peers: std::cell::RefCell<Vec<std::rc::Weak<crate::peer::PeerInner>>>,
}
#[derive(Clone)]
pub struct MediaStream(Rc<StreamInner>);
impl std::fmt::Debug for MediaStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaStream")
            .field("id", &self.id())
            .finish_non_exhaustive()
    }
}
impl MediaStream {
    pub(crate) fn from_native(
        native: cxx::UniquePtr<ffi::NativeMediaStream>,
        factory: Rc<FactoryInner>,
    ) -> Self {
        assert!(!native.is_null());
        Self(Rc::new(StreamInner {
            native,
            factory,
            tracks: Default::default(),
            peers: Default::default(),
        }))
    }
    pub(crate) fn factory(&self) -> &Rc<FactoryInner> {
        &self.0.factory
    }
    pub(crate) fn native(&self) -> &ffi::NativeMediaStream {
        self.0.native.as_ref().unwrap()
    }
    pub fn id(&self) -> Vec<u8> {
        ffi::media_stream_id(self.native())
    }
    pub(crate) fn register_peer(&self, peer: &Rc<crate::peer::PeerInner>) {
        let mut peers = self.0.peers.borrow_mut();
        peers.retain(|peer| peer.strong_count() != 0);
        if !peers.iter().any(|p| p.ptr_eq(&Rc::downgrade(peer))) {
            peers.push(Rc::downgrade(peer));
        }
    }
    pub(crate) fn source_leases(&self) -> Vec<MediaTrack> {
        self.0.tracks.borrow().clone()
    }
    fn refresh_peers(&self) {
        self.0
            .peers
            .borrow_mut()
            .retain(|peer| peer.strong_count() != 0);
        let peers: Vec<_> = self
            .0
            .peers
            .borrow()
            .iter()
            .filter_map(|p| p.upgrade())
            .collect();
        for peer in peers {
            peer.refresh_stream_inputs();
        }
    }
    /// Native AddTrack result. The source owner is retained only on acceptance.
    pub fn add_track(&self, track: &MediaTrack) -> Result<bool, crate::PeerError> {
        if !Rc::ptr_eq(self.factory(), track.factory()) {
            return Err(crate::PeerError {
                kind: crate::PeerErrorKind::InvalidParameter,
                message: "stream and track must belong to the same peer factory".into(),
            });
        }
        let accepted = ffi::media_stream_mutate_track(self.native(), track.native(), false);
        if accepted {
            self.0.tracks.borrow_mut().push(track.clone());
            self.refresh_peers();
        }
        Ok(accepted)
    }
    /// Native RemoveTrack result. This pin removes by track ID, even when the
    /// argument is a distinct SDK track. Source leases follow actual remaining
    /// SDK objects, not a guessed identity match against the argument.
    pub fn remove_track(&self, track: &MediaTrack) -> bool {
        let removed = ffi::media_stream_mutate_track(self.native(), track.native(), true);
        if removed {
            self.refresh_peers();
            let active: Vec<_> = self
                .audio_tracks()
                .into_iter()
                .chain(self.video_tracks())
                .collect();
            self.0
                .tracks
                .borrow_mut()
                .retain(|lease| active.iter().any(|value| value.same_resource(lease)));
        }
        removed
    }
    fn preserve_track_owner(&self, value: MediaTrack) -> MediaTrack {
        self.0
            .tracks
            .borrow()
            .iter()
            .find(|track| track.same_resource(&value))
            .cloned()
            .unwrap_or(value)
    }
    pub fn same_resource(&self, other: &Self) -> bool {
        ffi::media_stream_same_resource(self.native(), other.native())
    }
    fn tracks(&self, video: bool) -> Vec<MediaTrack> {
        let list = ffi::media_stream_tracks(self.native(), video);
        (0..ffi::media_tracks_len(&list))
            .map(|i| {
                self.preserve_track_owner(MediaTrack::from_native(
                    ffi::media_tracks_get(&list, i),
                    MediaTrackOwner::Factory {
                        _factory: self.0.factory.clone(),
                    },
                ))
            })
            .collect()
    }
    pub fn audio_tracks(&self) -> Vec<MediaTrack> {
        self.tracks(false)
    }
    pub fn video_tracks(&self) -> Vec<MediaTrack> {
        self.tracks(true)
    }
    fn find(&self, video: bool, id: &[u8]) -> Option<MediaTrack> {
        let native = ffi::media_stream_find(self.native(), video, id);
        (!native.is_null()).then(|| {
            self.preserve_track_owner(MediaTrack::from_native(
                native,
                MediaTrackOwner::Factory {
                    _factory: self.0.factory.clone(),
                },
            ))
        })
    }
    pub fn find_audio_track(&self, id: &[u8]) -> Option<MediaTrack> {
        self.find(false, id)
    }
    pub fn find_video_track(&self, id: &[u8]) -> Option<MediaTrack> {
        self.find(true, id)
    }
    pub fn observe(&self, capacity: NonZeroUsize) -> MediaObserver {
        MediaObserver {
            native: ffi::media_stream_observe(
                self.native(),
                self.0.factory.native(),
                capacity.get(),
            ),
            _owner: ObserverOwner::Stream {
                _stream: self.clone(),
            },
        }
    }
}
enum ObserverOwner {
    Track { _track: MediaTrack },
    Stream { _stream: MediaStream },
}
/// One actual argument-less SDK OnChanged notification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MediaChange;
/// An explicit notifier subscription with bounded pending notifications.
/// The source/track/stream lease outlives unregister and native destruction.
pub struct MediaObserver {
    native: cxx::UniquePtr<ffi::NativeMediaObserver>,
    _owner: ObserverOwner,
}
impl MediaObserver {
    pub fn take_event(&self) -> Option<MediaChange> {
        ffi::media_observer_take(self.native.as_ref().unwrap()).then_some(MediaChange)
    }
    pub fn take_dropped_events(&self) -> u64 {
        ffi::media_observer_take_dropped(self.native.as_ref().unwrap())
    }
}
