//! Owning stock SDK transformable frames. Frame ownership is independent of
//! callback delivery: retaining a frame does not retain its SDK callback queue.
use crate::{RtpVideoHeader, TimeDelta, Timestamp, VideoFrameMetadata, ffi};
use std::{
    cell::Cell,
    fmt,
    marker::PhantomData,
    ops::{Deref, DerefMut},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransformableFrameDirection {
    Unknown,
    Receiver,
    Sender,
    Other(i32),
}
impl TransformableFrameDirection {
    fn from_raw(value: i32) -> Self {
        match value {
            0 => Self::Unknown,
            1 => Self::Receiver,
            2 => Self::Sender,
            value => Self::Other(value),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransformableAudioFrameType {
    Empty,
    Speech,
    ComfortNoise,
    Other(i32),
}
impl TransformableAudioFrameType {
    fn from_raw(value: i32) -> Self {
        match value {
            0 => Self::Empty,
            1 => Self::Speech,
            2 => Self::ComfortNoise,
            value => Self::Other(value),
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransformableFrameError {
    UnsupportedSetter,
    /// These pinned SDK constructors unconditionally abort and cannot be called.
    UnsupportedVideoConstructor,
    UnsafeMimeCodec,
    UnsafeAudioCaptureTime,
    CodecVariantAssociation,
    ReceiverMetadataAssertion,
    ReceiverMetadataEqualityConsumption,
    /// The pinned SDK exposes no causal current GFD/DD consumer context.
    UnsupportedInstalledDependencyMutation,
}
impl fmt::Display for TransformableFrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UnsupportedSetter => "the stock frame does not support this setter",
            Self::UnsupportedVideoConstructor => {
                "the pinned SDK video frame constructor unconditionally aborts"
            }
            Self::UnsafeMimeCodec => {
                "unnamed video codec reaches the native MIME formatting assertion"
            }
            Self::UnsafeAudioCaptureTime => {
                "capture time cannot safely roundtrip through native unsigned audio milliseconds"
            }
            Self::CodecVariantAssociation => {
                "the selected native metadata conversion requires another codec variant"
            }
            Self::ReceiverMetadataAssertion => {
                "the compiled receiver setter permits only frame ID and dependency changes"
            }
            Self::UnsupportedInstalledDependencyMutation => "installed dependency metadata rewriting requires an unavailable native consumer-context hook",
            Self::ReceiverMetadataEqualityConsumption => {
                "receiver metadata assertion would read beyond native codec storage"
            }
        })
    }
}
impl std::error::Error for TransformableFrameError {}
fn status(value: i32) -> Result<(), TransformableFrameError> {
    match value {
        0 => Ok(()),
        1 => Err(TransformableFrameError::UnsupportedSetter),
        2 => Err(TransformableFrameError::UnsafeMimeCodec),
        3 => Err(TransformableFrameError::UnsafeAudioCaptureTime),
        4 => Err(TransformableFrameError::CodecVariantAssociation),
        5 => Err(TransformableFrameError::ReceiverMetadataAssertion),
        6 => Err(TransformableFrameError::ReceiverMetadataEqualityConsumption),
        7 => Err(TransformableFrameError::UnsupportedInstalledDependencyMutation),
        _ => unreachable!("closed frame bridge status"),
    }
}

/// Common owning SDK frame interface. Audio/video owners dereference to this
/// interface. The payload loan ends before any mutation. Frame owners may move
/// between application threads but do not permit concurrent shared use.
///
/// ```compile_fail
/// fn sync<T: Sync>() {} sync::<pulsebeam_webrtc_sys::TransformableFrame>();
/// ```
pub struct TransformableFrame {
    pub(crate) native: cxx::UniquePtr<ffi::NativeTransformableFrame>,
    _not_sync: PhantomData<Cell<()>>,
}
// SAFETY: stock frames own their values/refcounted encoded buffers. Live
// provenance additionally retains a synchronized unregister-barrier slot:
// SDK unregister releases its callback on the owning role, so late frame
// destruction cannot release a live callback or borrowed queue on this thread.
// Received-video producer pointers are comparison-only, never dereferenced by
// frame methods/destruction. Exclusive ownership serializes mutation.
unsafe impl Send for TransformableFrame {}
impl TransformableFrame {
    fn native(&self) -> &ffi::NativeTransformableFrame {
        self.native.as_ref().expect("owned stock frame")
    }
    pub fn data(&self) -> &[u8] {
        ffi::transformable_frame_data(self.native())
    }
    pub fn set_data(&mut self, data: &[u8]) {
        ffi::transformable_frame_set_data(self.native.pin_mut(), data);
    }
    pub fn payload_type(&self) -> u8 {
        ffi::transformable_frame_payload_type(self.native())
    }
    pub fn can_set_payload_type(&self) -> bool {
        ffi::transformable_frame_can_set_payload_type(self.native())
    }
    pub fn set_payload_type(&mut self, payload: u8) -> Result<(), TransformableFrameError> {
        if ffi::transformable_frame_set_payload_type(self.native.pin_mut(), payload) {
            Ok(())
        } else {
            Err(TransformableFrameError::UnsupportedSetter)
        }
    }
    pub fn ssrc(&self) -> u32 {
        ffi::transformable_frame_ssrc(self.native())
    }
    pub fn rtp_timestamp(&self) -> u32 {
        ffi::transformable_frame_timestamp(self.native())
    }
    pub fn set_rtp_timestamp(&mut self, timestamp: u32) {
        ffi::transformable_frame_set_timestamp(self.native.pin_mut(), timestamp);
    }
    pub fn direction(&self) -> TransformableFrameDirection {
        TransformableFrameDirection::from_raw(ffi::transformable_frame_direction(self.native()))
    }
    /// Native MIME bytes. No UTF-8 assumption or codec substitution.
    pub fn mime_type(&self) -> Result<Vec<u8>, TransformableFrameError> {
        let mut result = 0;
        let bytes = ffi::transformable_frame_mime(self.native(), &mut result);
        status(result)?;
        Ok(bytes)
    }
    fn time(&self, query: u8) -> Result<Option<Timestamp>, TransformableFrameError> {
        let value = ffi::transformable_frame_time(self.native(), query);
        status(value.status)?;
        Ok(value
            .present
            .then(|| Timestamp::from_native_microsecond_bits(value.bits)))
    }
    #[deprecated(note = "Use presentation_timestamp instead")]
    pub fn capture_time_identifier(&self) -> Result<Option<Timestamp>, TransformableFrameError> {
        self.time(0)
    }
    pub fn presentation_timestamp(&self) -> Result<Option<Timestamp>, TransformableFrameError> {
        self.time(1)
    }
    pub fn receive_time(&self) -> Result<Option<Timestamp>, TransformableFrameError> {
        self.time(2)
    }
    pub fn capture_time(&self) -> Result<Option<Timestamp>, TransformableFrameError> {
        self.time(3)
    }
    pub fn sender_capture_time_offset(&self) -> Result<Option<TimeDelta>, TransformableFrameError> {
        let value = ffi::transformable_frame_time(self.native(), 4);
        status(value.status)?;
        Ok(value
            .present
            .then(|| TimeDelta::from_native_microsecond_bits(value.bits)))
    }
    pub fn can_set_capture_time(&self) -> bool {
        ffi::transformable_frame_can_set_capture_time(self.native())
    }
    pub fn set_capture_time(
        &mut self,
        time: Option<Timestamp>,
    ) -> Result<(), TransformableFrameError> {
        status(ffi::transformable_frame_set_capture_time(
            self.native.pin_mut(),
            time.is_some(),
            time.map(Timestamp::native_microsecond_bits).unwrap_or(0),
        ))
    }
    /// Explicit SDK clone, not Rust Clone. Audio preserves native direction.
    /// Video always becomes a sender frame and resets or loses metadata exactly as
    /// CloneVideoFrame does, including its fresh capture/presentation fields.
    pub fn sdk_clone(&mut self) -> Result<TransformableMediaFrame, TransformableFrameError> {
        let mut result = 0;
        let frame = ffi::transformable_frame_clone(self.native.pin_mut(), &mut result);
        status(result)?;
        Ok(TransformableMediaFrame::from_native(frame))
    }
}
impl fmt::Debug for TransformableFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransformableFrame")
            .field("direction", &self.direction())
            .field("ssrc", &self.ssrc())
            .field("rtp_timestamp", &self.rtp_timestamp())
            .field("payload_bytes", &self.data().len())
            .finish()
    }
}

/// Owns one actual SDK audio or video frame, with typed subtype operations.
#[derive(Debug)]
pub enum TransformableMediaFrame {
    Audio(TransformableAudioFrame),
    Video(TransformableVideoFrame),
}
impl TransformableMediaFrame {
    pub(crate) fn into_native(mut self) -> cxx::UniquePtr<ffi::NativeTransformableFrame> {
        std::mem::replace(&mut self.native, cxx::UniquePtr::null())
    }
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeTransformableFrame>) -> Self {
        let audio =
            ffi::transformable_frame_is_audio(native.as_ref().expect("stock frame producer"));
        let frame = TransformableFrame {
            native,
            _not_sync: PhantomData,
        };
        if audio {
            Self::Audio(TransformableAudioFrame(frame))
        } else {
            Self::Video(TransformableVideoFrame(frame))
        }
    }
}
impl Deref for TransformableMediaFrame {
    type Target = TransformableFrame;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Audio(frame) => &frame.0,
            Self::Video(frame) => &frame.0,
        }
    }
}
impl DerefMut for TransformableMediaFrame {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            Self::Audio(frame) => &mut frame.0,
            Self::Video(frame) => &mut frame.0,
        }
    }
}
#[derive(Debug)]
pub struct TransformableAudioFrame(TransformableFrame);
impl Deref for TransformableAudioFrame {
    type Target = TransformableFrame;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl DerefMut for TransformableAudioFrame {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl TransformableAudioFrame {
    pub fn contributing_sources(&self) -> Vec<u32> {
        ffi::transformable_audio_csrcs(self.native())
    }
    pub fn sequence_number(&self) -> Option<u16> {
        let v = ffi::transformable_audio_fields(self.native());
        v.has_sequence.then_some(v.sequence)
    }
    /// Preserves the native uint64 value. Outgoing frames store milliseconds;
    /// incoming frames currently return None, rather than an inferred extension.
    pub fn absolute_capture_timestamp(&self) -> Option<u64> {
        let v = ffi::transformable_audio_fields(self.native());
        v.has_absolute_capture.then_some(v.absolute_capture)
    }
    pub fn frame_type(&self) -> TransformableAudioFrameType {
        TransformableAudioFrameType::from_raw(
            ffi::transformable_audio_fields(self.native()).frame_type,
        )
    }
    pub fn audio_level(&self) -> Option<u8> {
        let v = ffi::transformable_audio_fields(self.native());
        v.has_level.then_some(v.level)
    }
    pub fn can_set_audio_level(&self) -> bool {
        ffi::transformable_audio_can_set_level(self.native())
    }
    /// Uses the stock setter, including its clamp and receiver voice semantics.
    pub fn set_audio_level(&mut self, level: Option<u8>) -> Result<(), TransformableFrameError> {
        if ffi::transformable_audio_set_level(
            self.0.native.pin_mut(),
            level.is_some(),
            level.unwrap_or(0),
        ) {
            Ok(())
        } else {
            Err(TransformableFrameError::UnsupportedSetter)
        }
    }
}
#[derive(Debug)]
pub struct TransformableVideoFrame(TransformableFrame);
impl Deref for TransformableVideoFrame {
    type Target = TransformableFrame;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl DerefMut for TransformableVideoFrame {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl TransformableVideoFrame {
    pub fn create_sender() -> Result<Self, TransformableFrameError> {
        Err(TransformableFrameError::UnsupportedVideoConstructor)
    }
    pub fn create_receiver() -> Result<Self, TransformableFrameError> {
        Err(TransformableFrameError::UnsupportedVideoConstructor)
    }
    pub fn is_key_frame(&self) -> bool {
        ffi::transformable_video_is_key(self.native())
    }
    pub fn rid(&self) -> Option<Vec<u8>> {
        let mut present = false;
        let bytes = ffi::transformable_video_rid(self.native(), &mut present);
        present.then_some(bytes)
    }
    pub fn metadata(&self) -> Result<VideoFrameMetadata, TransformableFrameError> {
        let mut result = 0;
        let native = ffi::transformable_video_metadata(self.native(), &mut result);
        status(result)?;
        Ok(VideoFrameMetadata { native })
    }
    pub fn header(&self) -> RtpVideoHeader {
        RtpVideoHeader {
            native: ffi::transformable_video_header(self.native()),
        }
    }
    /// Native mutation only. Success does not certify packetization/delivery.
    /// Installed frames retain native-generated dependency fields: this SDK
    /// cannot safely certify arbitrary current GFD/DD dependency rewrites.
    /// Receiver frames update header ID/dependencies but retain their cached
    /// Metadata value, as the stock SDK does. Assertion-only restrictions depend
    /// on the compiled artifact, not on a binding-wide convenience policy.
    pub fn set_metadata(
        &mut self,
        metadata: &VideoFrameMetadata,
    ) -> Result<(), TransformableFrameError> {
        status(ffi::transformable_video_set_metadata(
            self.0.native.pin_mut(),
            metadata.native(),
        ))
    }
}
