use crate::ffi;

/// Exact signed dimensions used by native sink hints, not frame allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoSinkFrameSize {
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VideoSinkAggregates {
    pub any_active_without_requested_resolution: bool,
}

/// Complete SDK per-sink hints. Native sources decide which hints to honor;
/// registration does not promise resampling, cropping, rate limits or frames.
/// Track registration replaces `black_frames` with the track's enabled state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoSinkWants {
    pub rotation_applied: bool,
    pub black_frames: bool,
    pub max_pixel_count: i32,
    pub target_pixel_count: Option<i32>,
    pub max_framerate_fps: i32,
    pub resolution_alignment: i32,
    pub resolutions: Vec<VideoSinkFrameSize>,
    pub requested_resolution: Option<VideoSinkFrameSize>,
    pub is_active: bool,
    pub aggregates: Option<VideoSinkAggregates>,
}
impl Default for VideoSinkWants {
    fn default() -> Self {
        Self::from_native(ffi::video_sink_default_wants())
    }
}
impl VideoSinkWants {
    pub(crate) fn from_native(v: ffi::FfiVideoSinkWants) -> Self {
        Self {
            rotation_applied: v.rotation_applied,
            black_frames: v.black_frames,
            max_pixel_count: v.max_pixel_count,
            target_pixel_count: v.has_target_pixel_count.then_some(v.target_pixel_count),
            max_framerate_fps: v.max_framerate_fps,
            resolution_alignment: v.resolution_alignment,
            resolutions: v
                .resolutions
                .into_iter()
                .map(|size| VideoSinkFrameSize {
                    width: size.width,
                    height: size.height,
                })
                .collect(),
            requested_resolution: v.has_requested_resolution.then_some(VideoSinkFrameSize {
                width: v.requested_width,
                height: v.requested_height,
            }),
            is_active: v.is_active,
            aggregates: v.has_aggregates.then_some(VideoSinkAggregates {
                any_active_without_requested_resolution: v.any_active_without_requested_resolution,
            }),
        }
    }
    pub(crate) fn native(&self) -> ffi::FfiVideoSinkWants {
        ffi::FfiVideoSinkWants {
            valid: true,
            rotation_applied: self.rotation_applied,
            black_frames: self.black_frames,
            max_pixel_count: self.max_pixel_count,
            has_target_pixel_count: self.target_pixel_count.is_some(),
            target_pixel_count: self.target_pixel_count.unwrap_or(0),
            max_framerate_fps: self.max_framerate_fps,
            resolution_alignment: self.resolution_alignment,
            resolutions: self
                .resolutions
                .iter()
                .map(|size| ffi::FfiVideoSinkFrameSize {
                    width: size.width,
                    height: size.height,
                })
                .collect(),
            has_requested_resolution: self.requested_resolution.is_some(),
            requested_width: self.requested_resolution.map_or(0, |size| size.width),
            requested_height: self.requested_resolution.map_or(0, |size| size.height),
            is_active: self.is_active,
            has_aggregates: self.aggregates.is_some(),
            any_active_without_requested_resolution: self
                .aggregates
                .is_some_and(|v| v.any_active_without_requested_resolution),
        }
    }
}
