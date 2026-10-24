use crate::{
    ColorSpace, Timestamp, VideoCodecType, VideoRotation, VideoSinkEvent, VideoSourceView, ffi,
};
use std::rc::Rc;

/// Owned recordable source output. This is not a transformable RTP frame:
/// SSRC/RID/dependency fields unavailable on the SDK interface are not invented.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordableVideoFrame {
    /// None preserves a null native encoded buffer, distinct from an empty one.
    pub encoded_buffer: Option<Vec<u8>>,
    pub codec: VideoCodecType,
    pub key_frame: bool,
    pub width: u32,
    pub height: u32,
    pub rotation: Option<VideoRotation>,
    pub color_space: Option<ColorSpace>,
    pub render_time: Timestamp,
}
impl RecordableVideoFrame {
    /// The exact native EncodedResolution::empty predicate.
    pub fn resolution_is_empty(&self) -> bool {
        self.width == 0 && self.height == 0
    }
    fn from_native(v: ffi::FfiRecordableVideoFrame) -> Self {
        Self {
            encoded_buffer: v.has_buffer.then_some(v.data),
            codec: VideoCodecType::from_native(v.codec),
            key_frame: v.key_frame,
            width: v.width,
            height: v.height,
            rotation: v
                .has_rotation
                .then(|| VideoRotation::from_degrees(v.rotation)),
            color_space: v
                .has_color_space
                .then(|| ColorSpace::from_native(v.color_space)),
            render_time: Timestamp::from_native_microsecond_bits(v.render_time_us),
        }
    }
}

/// Owning source encoded-output registration. Independent registrations are
/// supported, unlike the receiver's once-per-lifetime transformer interception.
/// Clones share one registration, eight-frame/64 MiB queue and close state.
/// Overflow discards oldest frames, or an individual frame exceeding the budget.
#[derive(Clone)]
pub struct VideoSourceEncodedSink {
    inner: Rc<Inner>,
}
struct Inner {
    // Remove the native callback before releasing source/factory dependencies.
    native: cxx::UniquePtr<ffi::NativeVideoSourceEncodedSink>,
    _source: VideoSourceView,
}
impl VideoSourceEncodedSink {
    pub(crate) fn new(
        native: cxx::UniquePtr<ffi::NativeVideoSourceEncodedSink>,
        source: VideoSourceView,
    ) -> Self {
        Self {
            inner: Rc::new(Inner {
                native,
                _source: source,
            }),
        }
    }
    fn native(&self) -> &ffi::NativeVideoSourceEncodedSink {
        self.inner
            .native
            .as_ref()
            .expect("owning source encoded sink")
    }
    pub fn try_next_frame(&self) -> Option<RecordableVideoFrame> {
        let value = ffi::video_source_encoded_take_frame(self.native());
        value
            .valid
            .then(|| RecordableVideoFrame::from_native(value))
    }
    pub fn try_next_event(&self) -> Option<VideoSinkEvent> {
        VideoSinkEvent::from_native(ffi::video_source_encoded_take_event(self.native()))
    }
    pub fn dropped_frames(&self) -> u64 {
        ffi::video_source_encoded_dropped_frames(self.native())
    }
    pub fn dropped_events(&self) -> u64 {
        ffi::video_source_encoded_dropped_events(self.native())
    }
    /// Native removal waits for in-flight callbacks. Accepted control events
    /// and loss counts remain available; queued frame buffers are released.
    pub fn close(&self) {
        ffi::video_source_encoded_close(self.native());
    }
}
