//! Independently owned SDK receiver source observations.
use crate::{AbsoluteCaptureTime, TimeDelta, Timestamp, ffi};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RtpSourceType {
    Ssrc,
    Csrc,
    Other(i32),
}
impl RtpSourceType {
    pub const fn to_native(self) -> i32 {
        match self {
            Self::Ssrc => 0,
            Self::Csrc => 1,
            Self::Other(value) => value,
        }
    }
    pub const fn from_native(value: i32) -> Self {
        match value {
            0 => Self::Ssrc,
            1 => Self::Csrc,
            value => Self::Other(value),
        }
    }
}

/// Full SDK RtpSource::Extensions. The audio level is an unrestricted uint8
/// metadata value, not the separately checked RTP header AudioLevel type.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RtpSourceExtensions {
    pub audio_level: Option<u8>,
    pub absolute_capture_time: Option<AbsoluteCaptureTime>,
    /// Local/capture clock offset, not the remote sender/capture clock offset.
    pub local_capture_clock_offset: Option<TimeDelta>,
}

/// Actual owned SDK RtpSource. There is no native default constructor.
/// Native equality intentionally omits the local capture clock offset.
pub struct RtpSource {
    native: cxx::UniquePtr<ffi::NativeRtpSource>,
}
// SAFETY: RtpSource owns only pure value metadata, with no callbacks, borrowed
// resources, cached mutation, origin sequence, or external references. Shared
// reads copy metadata; mutation requires an exclusive loan.
unsafe impl Send for RtpSource {}
unsafe impl Sync for RtpSource {}
impl RtpSource {
    pub fn new(
        timestamp: Timestamp,
        source_id: u32,
        source_type: RtpSourceType,
        rtp_timestamp: u32,
        extensions: RtpSourceExtensions,
    ) -> Self {
        let capture = extensions
            .absolute_capture_time
            .unwrap_or(AbsoluteCaptureTime {
                absolute_capture_timestamp: 0,
                estimated_capture_clock_offset: None,
            });
        Self::from_native(ffi::rtp_source_new(&ffi::FfiRtpSourceFields {
            timestamp_us: timestamp.native_microsecond_bits(),
            source_id,
            source_type: source_type.to_native(),
            rtp_timestamp,
            has_audio_level: extensions.audio_level.is_some(),
            audio_level: extensions.audio_level.unwrap_or(0),
            has_absolute_capture_time: extensions.absolute_capture_time.is_some(),
            absolute_capture_time: capture.to_native(),
            has_local_capture_clock_offset: extensions.local_capture_clock_offset.is_some(),
            local_capture_clock_offset_us: extensions
                .local_capture_clock_offset
                .map(TimeDelta::native_microsecond_bits)
                .unwrap_or(0),
        }))
    }
    pub(crate) fn from_native(native: cxx::UniquePtr<ffi::NativeRtpSource>) -> Self {
        assert!(!native.is_null());
        Self { native }
    }
    pub(crate) fn native(&self) -> &ffi::NativeRtpSource {
        self.native.as_ref().unwrap()
    }
    fn fields(&self) -> ffi::FfiRtpSourceFields {
        ffi::rtp_source_fields(self.native())
    }
    pub fn timestamp(&self) -> Timestamp {
        Timestamp::from_native_microsecond_bits(self.fields().timestamp_us)
    }
    pub fn source_id(&self) -> u32 {
        self.fields().source_id
    }
    pub fn source_type(&self) -> RtpSourceType {
        RtpSourceType::from_native(self.fields().source_type)
    }
    pub fn rtp_timestamp(&self) -> u32 {
        self.fields().rtp_timestamp
    }
    pub fn audio_level(&self) -> Option<u8> {
        let fields = self.fields();
        fields.has_audio_level.then_some(fields.audio_level)
    }
    pub fn set_audio_level(&mut self, level: Option<u8>) {
        ffi::rtp_source_set_audio_level(self.native.pin_mut(), level.is_some(), level.unwrap_or(0));
    }
    pub fn absolute_capture_time(&self) -> Option<AbsoluteCaptureTime> {
        let fields = self.fields();
        fields
            .has_absolute_capture_time
            .then(|| AbsoluteCaptureTime::from_native(fields.absolute_capture_time))
    }
    pub fn local_capture_clock_offset(&self) -> Option<TimeDelta> {
        let fields = self.fields();
        fields
            .has_local_capture_clock_offset
            .then(|| TimeDelta::from_native_microsecond_bits(fields.local_capture_clock_offset_us))
    }
    pub fn extensions(&self) -> RtpSourceExtensions {
        let fields = self.fields();
        RtpSourceExtensions {
            audio_level: fields.has_audio_level.then_some(fields.audio_level),
            absolute_capture_time: fields
                .has_absolute_capture_time
                .then(|| AbsoluteCaptureTime::from_native(fields.absolute_capture_time)),
            local_capture_clock_offset: fields.has_local_capture_clock_offset.then(|| {
                TimeDelta::from_native_microsecond_bits(fields.local_capture_clock_offset_us)
            }),
        }
    }
    pub fn native_string_bytes(&self) -> Vec<u8> {
        ffi::rtp_source_string(self.native())
    }
}
impl Clone for RtpSource {
    fn clone(&self) -> Self {
        Self::from_native(ffi::rtp_source_clone(self.native()))
    }
    fn clone_from(&mut self, source: &Self) {
        ffi::rtp_source_assign(self.native.pin_mut(), source.native());
    }
}
impl PartialEq for RtpSource {
    fn eq(&self, other: &Self) -> bool {
        ffi::rtp_source_equal(self.native(), other.native())
    }
}
impl Eq for RtpSource {}
impl std::fmt::Debug for RtpSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RtpSource")
            .field("timestamp", &self.timestamp())
            .field("source_id", &self.source_id())
            .field("source_type", &self.source_type())
            .field("rtp_timestamp", &self.rtp_timestamp())
            .field("extensions", &self.extensions())
            .finish()
    }
}
