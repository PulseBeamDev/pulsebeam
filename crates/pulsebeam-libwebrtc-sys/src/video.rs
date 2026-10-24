use std::{cell::Cell, fmt, rc::Rc};

use crate::{
    AudioCodecCapability, CodecError, VideoCodecFormat, VideoFrame, VideoResolution, ffi,
    peer::{FactoryInner, PeerError, PeerErrorKind, PeerInner, error_kind},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum RtpTransceiverDirection {
    SendReceive = 0,
    SendOnly = 1,
    ReceiveOnly = 2,
    Inactive = 3,
}

/// Native direction of a negotiable RTP header extension.
/// `Stopped` disables advertisement without stopping the transceiver.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum RtpHeaderExtensionDirection {
    SendReceive = 0,
    SendOnly = 1,
    ReceiveOnly = 2,
    Inactive = 3,
    Stopped = 4,
}

/// Native extension capability snapshot. Only its direction is configurable.
/// Preserve the complete ordered vector when applying negotiation preferences.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtpHeaderExtensionCapability {
    uri: String,
    preferred_id: Option<i32>,
    preferred_encrypt: bool,
    pub direction: RtpHeaderExtensionDirection,
}

impl RtpHeaderExtensionCapability {
    pub fn uri(&self) -> &str {
        &self.uri
    }
    pub fn preferred_id(&self) -> Option<i32> {
        self.preferred_id
    }
    pub fn preferred_encrypt(&self) -> bool {
        self.preferred_encrypt
    }
}

/// An actual video RTP capability reported by the pinned peer engine.
/// Resiliency codecs (such as RTX) may also appear in the list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoCodecCapability {
    format: VideoCodecFormat,
    clock_rate: Option<u32>,
    preferred_payload_type: Option<u8>,
    rtcp_feedback: Vec<String>,
    scalability_modes: Vec<String>,
}

impl VideoCodecCapability {
    pub fn format(&self) -> &VideoCodecFormat {
        &self.format
    }
    pub fn clock_rate(&self) -> Option<u32> {
        self.clock_rate
    }
    pub fn preferred_payload_type(&self) -> Option<u8> {
        self.preferred_payload_type
    }
    pub fn rtcp_feedback(&self) -> &[String] {
        &self.rtcp_feedback
    }
    pub fn scalability_modes(&self) -> &[String] {
        &self.scalability_modes
    }

    pub(crate) fn from_ffi(value: ffi::FfiVideoCodecCapability) -> Result<Self, crate::PeerError> {
        let owned = crate::SdpVideoFormat::from_native(value.format);
        let format = VideoCodecFormat::try_from(&owned).map_err(|error| crate::PeerError {
            kind: crate::PeerErrorKind::UnrepresentableCodecCapability,
            message: format!("legacy capability projection failed: {error}; use get_rtp_sender_capabilities/get_rtp_receiver_capabilities"),
        })?;
        let scalability_modes = format
            .scalability_modes
            .iter()
            .map(|mode| mode.as_str().to_owned())
            .collect();
        Ok(Self {
            format,
            clock_rate: u32::try_from(value.clock_rate).ok(),
            preferred_payload_type: u8::try_from(value.preferred_payload_type).ok(),
            rtcp_feedback: value.rtcp_feedback,
            scalability_modes,
        })
    }

    fn ffi_format(&self) -> ffi::FfiCodecFormat {
        ffi::FfiCodecFormat {
            name: self.format.name.clone(),
            scalability_modes: self
                .format
                .scalability_modes
                .iter()
                .map(|mode| mode.as_str().into())
                .collect(),
            parameters: self
                .format
                .parameters
                .iter()
                .map(|item| ffi::FfiCodecParameter {
                    key: item.key.clone(),
                    value: item.value.clone(),
                })
                .collect(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoSourceState {
    Initializing,
    Live,
    Ended,
    Muted,
}
impl VideoSourceState {
    fn from_native(value: u8) -> Self {
        match value {
            0 => Self::Initializing,
            1 => Self::Live,
            2 => Self::Ended,
            3 => Self::Muted,
            value => unreachable!("native adapter returned invalid video source state {value}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoContentHint {
    None,
    Fluid,
    Detailed,
    Text,
}
impl VideoContentHint {
    fn native(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Fluid => 1,
            Self::Detailed => 2,
            Self::Text => 3,
        }
    }
    fn from_native(value: u8) -> Self {
        match value {
            0 => Self::None,
            1 => Self::Fluid,
            2 => Self::Detailed,
            3 => Self::Text,
            value => unreachable!("native adapter returned invalid content hint {value}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoSourceStats {
    pub input_width: i32,
    pub input_height: i32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VideoSourceStatus {
    pub state: VideoSourceState,
    pub remote: bool,
    pub is_screencast: bool,
    pub needs_denoising: Option<bool>,
    pub supports_encoded_output: bool,
    pub stats: Option<VideoSourceStats>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VideoTrackStatus {
    pub id: String,
    pub kind: String,
    pub state: VideoTrackState,
    pub enabled: bool,
    pub should_receive: bool,
    pub content_hint: VideoContentHint,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoTrackState {
    Live,
    Ended,
}

/// A platform screen identified by WebRTC's desktop capture module.
#[cfg(feature = "native")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScreenSource {
    pub id: i64,
    pub name: String,
    /// Native display identity, including the SDK invalid-display sentinel.
    pub display_id: i64,
}

/// A window shares the desktop capturer's ID and title representation. On
/// Wayland, IDs can be portal placeholders rather than unrestricted windows.
#[cfg(feature = "native")]
pub type WindowSource = ScreenSource;

/// A window uses the same caller-paced capture and lifecycle contract.
#[cfg(feature = "native")]
pub type WindowCapture = ScreenCapture;

/// Native screen capture lifecycle. On Wayland, the selection UI is owned by
/// the desktop portal and may remain pending until the user responds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(feature = "native")]
pub enum ScreenCaptureStatus {
    PendingSelection,
    Selected,
    Streaming,
    /// The portal reported user cancellation (which may include a denial).
    CancelledOrDenied,
    /// Upstream's generic error; missing service, denial, and capture errors
    /// cannot be distinguished through this callback.
    Failed,
    /// The Wayland portal closed an active session (not the same as `stop`).
    PortalSessionClosed,
    Closed,
}

/// Caller-paced native screen capture. Call [`Self::capture_next_frame`] to
/// request each frame, then use [`Self::source`] to attach a video track.
#[cfg(feature = "native")]
pub struct ScreenCapture {
    native: cxx::UniquePtr<ffi::NativeScreenCapture>,
    source: VideoSource,
}

#[cfg(feature = "native")]
impl ScreenCapture {
    pub(crate) fn new(
        native: cxx::UniquePtr<ffi::NativeScreenCapture>,
        source: VideoSource,
    ) -> Self {
        Self { native, source }
    }

    pub fn source(&self) -> &VideoSource {
        &self.source
    }

    fn control(
        &self,
        kind: u8,
        id: i64,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> Result<ffi::FfiDesktopControl, PeerError> {
        self.source.inner._factory.check_control_call()?;
        let reply = ffi::desktop_control(
            self.native.as_ref().expect("validated desktop capture"),
            kind,
            id,
            x,
            y,
            width,
            height,
        );
        let kind = match reply.status {
            1 => Some(PeerErrorKind::Closed),
            2 => Some(PeerErrorKind::UnsupportedOperation),
            3 => Some(PeerErrorKind::InvalidParameter),
            _ => None,
        };
        if let Some(kind) = kind {
            return Err(PeerError {
                kind,
                message: "desktop control unavailable".into(),
            });
        }
        Ok(reply)
    }
    pub fn sources(&self) -> Result<Vec<ScreenSource>, PeerError> {
        let reply = self.control(0, 0, 0, 0, 0, 0)?;
        if !reply.flag {
            return Err(PeerError {
                kind: PeerErrorKind::Operation,
                message: "native desktop source enumeration failed".into(),
            });
        }
        Ok(reply
            .sources
            .into_iter()
            .map(|source| ScreenSource {
                id: source.id,
                name: source.name,
                display_id: source.display_id,
            })
            .collect())
    }
    /// Preserve native rejection as false; never select another source.
    pub fn select_source(&mut self, id: i64) -> Result<bool, PeerError> {
        Ok(self.control(1, id, 0, 0, 0, 0)?.flag)
    }
    pub fn focus_selected_source(&mut self) -> Result<bool, PeerError> {
        Ok(self.control(2, 0, 0, 0, 0, 0)?.flag)
    }
    pub fn is_occluded(&self, x: i32, y: i32) -> Result<bool, PeerError> {
        Ok(self.control(3, 0, x, y, 0, 0)?.flag)
    }
    pub fn set_excluded_window(&mut self, id: i64) -> Result<(), PeerError> {
        self.control(4, id, 0, 0, 0, 0).map(|_| ())
    }
    pub fn set_max_frame_rate(&mut self, rate: u32) -> Result<(), PeerError> {
        self.control(5, 0, 0, 0, rate, 0).map(|_| ())
    }
    pub fn update_resolution(&mut self, width: u32, height: u32) -> Result<(), PeerError> {
        self.control(6, 0, 0, 0, width, height).map(|_| ())
    }
    pub fn has_delegated_selection(&self) -> Result<bool, PeerError> {
        Ok(self.control(7, 0, 0, 0, 0, 0)?.flag)
    }
    /// These operations request visibility, not a claim that UI was displayed.
    pub fn ensure_selection_visible(&mut self) -> Result<(), PeerError> {
        self.control(8, 0, 0, 0, 0, 0).map(|_| ())
    }
    pub fn ensure_selection_hidden(&mut self) -> Result<(), PeerError> {
        self.control(9, 0, 0, 0, 0, 0).map(|_| ())
    }
    /// Clear or restore the native observer backing selection status. With it
    /// disabled, status retains its last notification rather than polling UI.
    pub fn set_selection_observation(&mut self, enabled: bool) -> Result<(), PeerError> {
        self.control(if enabled { 10 } else { 11 }, 0, 0, 0, 0, 0)
            .map(|_| ())
    }
    /// SDK GetMetadata. None means the artifact has no GIO metadata interface;
    /// a supported backend's actual empty value is Some, never fabricated data.
    pub fn metadata(&self) -> Result<Option<crate::DesktopCaptureMetadata>, PeerError> {
        self.source.inner._factory.check_control_call()?;
        if self.status() == ScreenCaptureStatus::Closed {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "desktop capture is closed".into(),
            });
        }
        let native =
            ffi::desktop_capture_metadata(self.native.as_ref().expect("validated desktop capture"));
        Ok((!native.is_null()).then(|| crate::DesktopCaptureMetadata::new(native)))
    }
    /// Pinned Linux capturers inherit a no-op SDK setter. Some is explicitly
    /// UnsupportedOperation rather than fake installation or a stock allocator.
    /// None calls the native optional setter without claiming allocation support.
    pub fn set_shared_memory_factory(
        &mut self,
        factory: Option<&crate::DesktopMemoryFactoryHandle>,
    ) -> Result<(), PeerError> {
        self.source.inner._factory.check_control_call()?;
        let status = ffi::desktop_set_memory_factory(
            self.native.as_ref().expect("validated desktop capture"),
            crate::desktop_memory::RustDesktopMemoryFactory::new(factory),
        );
        Self::callback_status(status)
    }
    /// Replace the SDK frame consumer. Some bypasses ordinary video publication;
    /// None restores it. Replacement is atomic per event, not per pending request.
    /// Already executing callbacks retain their previous registration.
    pub fn set_receiver(
        &mut self,
        receiver: Option<&crate::DesktopCaptureReceiverHandle>,
    ) -> Result<(), PeerError> {
        self.source.inner._factory.check_control_call()?;
        let status = ffi::desktop_set_receiver(
            self.native.as_ref().expect("validated desktop capture"),
            crate::desktop_receiver::RustDesktopCaptureReceiver::new(receiver),
        );
        Self::callback_status(status)
    }
    /// Calls SDK Observe(nullptr) before registering a new observer. None clears
    /// native observation, including automatic selection-status updates.
    pub fn set_selection_observer(
        &mut self,
        observer: Option<&crate::DesktopSelectionObserverHandle>,
    ) -> Result<(), PeerError> {
        self.source.inner._factory.check_control_call()?;
        let status = ffi::desktop_set_selection_observer(
            self.native.as_ref().expect("validated desktop capture"),
            crate::desktop_receiver::RustDesktopSelectionObserver::new(observer),
        );
        Self::callback_status(status)
    }
    fn callback_status(status: u8) -> Result<(), PeerError> {
        match status {
            0 => Ok(()),
            1 => Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "desktop capture is closed".into(),
            }),
            _ => Err(PeerError {
                kind: PeerErrorKind::UnsupportedOperation,
                message: "SDK desktop callback operation unavailable".into(),
            }),
        }
    }
    /// Call SDK CaptureFrame without filtering terminal selection/capture
    /// statuses. Acceptance is not capture success; the actual result is sent
    /// to the selected receiver, or processed by the ordinary video sink.
    pub fn request_frame(&self) -> Result<(), PeerError> {
        self.source.inner._factory.check_control_call()?;
        if ffi::screen_capture_next_frame(self.native.as_ref().expect("validated desktop capture"))
        {
            Ok(())
        } else {
            Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "desktop capture is closed".into(),
            })
        }
    }
    /// Convenience policy rejects terminal statuses before requesting a frame.
    /// Capture and conversion failures are accumulated by [`Self::failed_frames`].
    pub fn capture_next_frame(&self) -> Result<(), PeerError> {
        self.source.inner._factory.check_control_call()?;
        let native = self.native.as_ref().expect("validated desktop capture");
        match self.status() {
            ScreenCaptureStatus::Closed => {
                return Err(PeerError {
                    kind: PeerErrorKind::Closed,
                    message: "desktop capture is closed".into(),
                });
            }
            ScreenCaptureStatus::CancelledOrDenied => {
                return Err(PeerError {
                    kind: PeerErrorKind::Operation,
                    message: "desktop selection was cancelled or denied".into(),
                });
            }
            ScreenCaptureStatus::Failed => {
                return Err(PeerError {
                    kind: PeerErrorKind::Operation,
                    message: "desktop capture failed".into(),
                });
            }
            ScreenCaptureStatus::PortalSessionClosed => {
                return Err(PeerError {
                    kind: PeerErrorKind::Operation,
                    message: "desktop portal session closed".into(),
                });
            }
            _ => {}
        }
        if !ffi::screen_capture_next_frame(native) {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "desktop capture is closed".into(),
            });
        }
        match self.status() {
            ScreenCaptureStatus::Failed => Err(PeerError {
                kind: PeerErrorKind::Operation,
                message: "desktop capture failed".into(),
            }),
            ScreenCaptureStatus::PortalSessionClosed => Err(PeerError {
                kind: PeerErrorKind::Operation,
                message: "desktop portal session closed".into(),
            }),
            _ => Ok(()),
        }
    }

    /// Portal selection and capture state. `CancelledOrDenied`, `Failed`, and
    /// `PortalSessionClosed` are terminal; check with [`Self::failed_frames`] after requesting
    /// frames. Headless X11 selections begin in `Selected`.
    pub fn status(&self) -> ScreenCaptureStatus {
        match ffi::screen_capture_status(self.native.as_ref().expect("validated screen capture")) {
            0 => ScreenCaptureStatus::PendingSelection,
            1 => ScreenCaptureStatus::Selected,
            2 => ScreenCaptureStatus::Streaming,
            3 => ScreenCaptureStatus::CancelledOrDenied,
            4 => ScreenCaptureStatus::Failed,
            6 => ScreenCaptureStatus::PortalSessionClosed,
            _ => ScreenCaptureStatus::Closed,
        }
    }

    /// Number of failed capture callbacks or frames rejected before delivery.
    pub fn failed_frames(&self) -> u64 {
        ffi::screen_capture_failed_frames(self.native.as_ref().expect("validated screen capture"))
    }

    /// Stop capture and end the associated video source. Repeated stops succeed.
    pub fn stop(&mut self) -> Result<(), PeerError> {
        self.source.inner._factory.check_control_call()?;
        let stopped = self.native.as_ref().is_some_and(ffi::close_screen);
        let source_stopped = self.source.close().is_ok();
        (stopped && source_stopped)
            .then_some(())
            .ok_or_else(|| PeerError {
                kind: PeerErrorKind::Operation,
                message: "failed to stop desktop capture or end its video source".into(),
            })
    }
}

#[cfg(feature = "native")]
impl Drop for ScreenCapture {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Pixel layout offered by a platform capture device. Formats other than
/// I420/NV12 are converted to I420 by WebRTC before track delivery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(feature = "native")]
pub enum CameraPixelFormat {
    Unknown,
    I420,
    Iyuv,
    Rgb24,
    Bgr24,
    Argb,
    Abgr,
    Rgb565,
    Yuy2,
    Yv12,
    Uyvy,
    Mjpeg,
    Bgra,
    Nv12,
    /// Unknown signed scoped-enum bits, retained without normalization.
    UnknownValue(i32),
}

#[cfg(feature = "native")]
impl CameraPixelFormat {
    pub(crate) fn from_native(value: i32) -> Self {
        match value {
            1 => Self::I420,
            2 => Self::Iyuv,
            3 => Self::Rgb24,
            4 => Self::Bgr24,
            5 => Self::Argb,
            6 => Self::Abgr,
            7 => Self::Rgb565,
            8 => Self::Yuy2,
            9 => Self::Yv12,
            10 => Self::Uyvy,
            11 => Self::Mjpeg,
            12 => Self::Bgra,
            13 => Self::Nv12,
            0 => Self::Unknown,
            value => Self::UnknownValue(value),
        }
    }
    pub fn native_bits(self) -> i32 {
        match self {
            Self::Unknown => 0,
            Self::I420 => 1,
            Self::Iyuv => 2,
            Self::Rgb24 => 3,
            Self::Bgr24 => 4,
            Self::Argb => 5,
            Self::Abgr => 6,
            Self::Rgb565 => 7,
            Self::Yuy2 => 8,
            Self::Yv12 => 9,
            Self::Uyvy => 10,
            Self::Mjpeg => 11,
            Self::Bgra => 12,
            Self::Nv12 => 13,
            Self::UnknownValue(value) => value,
        }
    }
}

/// Exact pinned VideoCaptureCapability, including signed settings and interlace.
#[cfg(feature = "native")]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CameraSettings {
    pub width: i32,
    pub height: i32,
    pub max_fps: i32,
    pub pixel_format: CameraPixelFormat,
    pub interlaced: bool,
}
#[cfg(feature = "native")]
impl Default for CameraSettings {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            max_fps: 0,
            pixel_format: CameraPixelFormat::Unknown,
            interlaced: false,
        }
    }
}
#[cfg(feature = "native")]
impl CameraSettings {
    pub(crate) fn native(&self) -> ffi::FfiCameraSettings {
        ffi::FfiCameraSettings {
            width: self.width,
            height: self.height,
            max_fps: self.max_fps,
            pixel_format: self.pixel_format.native_bits(),
            interlaced: self.interlaced,
        }
    }
    pub(crate) fn from_native(value: ffi::FfiCameraSettings) -> Self {
        Self {
            width: value.width,
            height: value.height,
            max_fps: value.max_fps,
            pixel_format: CameraPixelFormat::from_native(value.pixel_format),
            interlaced: value.interlaced,
        }
    }
}
#[cfg(feature = "native")]
#[derive(Debug)]
pub enum CameraControlError {
    Binding(PeerError),
    Closed,
    Unsupported,
    InvalidSettings,
    SinkFailed,
    Native(i32),
}
#[cfg(feature = "native")]
impl CameraControlError {
    pub(crate) fn check(status: u8, native_result: i32) -> Result<(), Self> {
        match status {
            1 => Err(Self::Closed),
            2 => Err(Self::Unsupported),
            3 => Err(Self::InvalidSettings),
            _ if native_result != 0 => Err(Self::Native(native_result)),
            _ => Ok(()),
        }
    }
}
#[cfg(feature = "native")]
impl std::fmt::Display for CameraControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "camera control failed: {self:?}")
    }
}
#[cfg(feature = "native")]
impl std::error::Error for CameraControlError {}

/// A capture format advertised by a native camera.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg(feature = "native")]
pub struct CameraFormat {
    pub width: u32,
    pub height: u32,
    pub max_fps: u32,
    pub pixel_format: CameraPixelFormat,
}

/// A native camera name and stable device identifier reported by WebRTC.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg(feature = "native")]
pub struct CameraDevice {
    pub name: String,
    pub id: String,
    pub product_id: String,
}

/// Observable camera delivery state. `Stalled` means no frame arrived within
/// the caller's timeout, not a diagnosed reason for device loss.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg(feature = "native")]
pub enum CameraCaptureStatus {
    Starting,
    Streaming,
    Stalled,
    Closed,
    Failed,
    /// Sampling would require a forbidden synchronous SDK role hop. This is
    /// not device failure; sample again from an ordinary control context.
    ControlUnavailable,
    /// Explicit StopCapture succeeded; the source is not terminally closed.
    Stopped,
}

/// A running camera bound to a caller-owned video source. Capture stops on
/// drop; the source remains usable for a track while this handle is alive.
#[cfg(feature = "native")]
pub struct CameraCapture {
    native: cxx::UniquePtr<ffi::NativeCamera>,
    source: VideoSource,
    _options: Option<crate::CameraCaptureOptions>,
}

#[cfg(feature = "native")]
impl CameraCapture {
    pub(crate) fn new(native: cxx::UniquePtr<ffi::NativeCamera>, source: VideoSource) -> Self {
        Self {
            native,
            source,
            _options: None,
        }
    }

    pub(crate) fn with_options(mut self, options: crate::CameraCaptureOptions) -> Self {
        self._options = Some(options);
        self
    }

    pub fn source(&self) -> &VideoSource {
        &self.source
    }

    fn control(
        &self,
        kind: u8,
        rotation: u16,
        flag: bool,
        settings: &CameraSettings,
    ) -> Result<ffi::FfiCameraControl, CameraControlError> {
        self.source
            .inner
            ._factory
            .check_control_call()
            .map_err(CameraControlError::Binding)?;
        let reply = ffi::camera_control(
            self.native.as_ref().expect("validated camera"),
            kind,
            rotation,
            flag,
            &settings.native(),
        );
        CameraControlError::check(reply.status, reply.native_result)?;
        Ok(reply)
    }
    /// Native CaptureSettings output, not an inferred hardware configuration.
    pub fn settings(&self) -> Result<CameraSettings, CameraControlError> {
        Ok(CameraSettings::from_native(
            self.control(0, 0, false, &CameraSettings::default())?
                .settings,
        ))
    }
    pub fn current_device_name(&self) -> Result<String, CameraControlError> {
        Ok(self
            .control(1, 0, false, &CameraSettings::default())?
            .device_name)
    }
    pub fn capture_started(&self) -> Result<bool, CameraControlError> {
        Ok(self.control(2, 0, false, &CameraSettings::default())?.flag)
    }
    pub fn set_capture_rotation(
        &mut self,
        rotation: crate::VideoRotation,
    ) -> Result<(), CameraControlError> {
        self.control(3, rotation as u16, false, &CameraSettings::default())
            .map(|_| ())
    }
    /// Preserve the native success boolean, including ordinary false.
    pub fn set_apply_rotation(&mut self, enabled: bool) -> Result<bool, CameraControlError> {
        Ok(self
            .control(4, 0, enabled, &CameraSettings::default())?
            .flag)
    }
    pub fn apply_rotation(&self) -> Result<bool, CameraControlError> {
        Ok(self.control(5, 0, false, &CameraSettings::default())?.flag)
    }
    /// Stop only the device. Unlike stop(), this does not end the video source.
    pub fn stop_device(&mut self) -> Result<(), CameraControlError> {
        self.control(6, 0, false, &CameraSettings::default())
            .map(|_| ())
    }
    /// Replace the SDK callback mode using deregister/register, not an atomic
    /// swap. Frames in the gap may be discarded. Some installs a mutable raw
    /// receiver, bypassing video-source delivery; None restores the video sink
    /// which publishes frames into source(). Callback return codes are ignored
    /// by this SDK. Deregistration fences in-flight callbacks before old Drop.
    pub fn set_raw_sink(
        &mut self,
        sink: Option<crate::RawCameraSinkHandle>,
    ) -> Result<(), CameraControlError> {
        self.source
            .inner
            ._factory
            .check_control_call()
            .map_err(CameraControlError::Binding)?;
        if sink
            .as_ref()
            .is_some_and(crate::RawCameraSinkHandle::has_failed)
        {
            return Err(CameraControlError::SinkFailed);
        }
        let status = ffi::camera_set_raw_sink(
            self.native.as_ref().expect("validated camera"),
            crate::camera_sink::RustRawCameraSink::new(sink),
        );
        CameraControlError::check(status, 0)
    }
    /// Forward complete native settings without choosing another capability.
    pub fn start_device(&mut self, settings: &CameraSettings) -> Result<(), CameraControlError> {
        self.control(7, 0, false, settings).map(|_| ())
    }
    /// Detect stopped delivery after `stale_after` without assuming the cause.
    /// A capture may be `Starting` until its first frame, or `Stalled` if no
    /// frame arrives in time (including after a device disappears).
    pub fn status(&self, stale_after: std::time::Duration) -> CameraCaptureStatus {
        if self.source.inner._factory.check_control_call().is_err() {
            return CameraCaptureStatus::ControlUnavailable;
        }
        let millis = stale_after.as_millis().min(u128::from(u64::MAX)) as u64;
        match ffi::camera_capture_status(self.native.as_ref().expect("validated camera"), millis) {
            0 => CameraCaptureStatus::Starting,
            1 => CameraCaptureStatus::Streaming,
            2 => CameraCaptureStatus::Stalled,
            3 => CameraCaptureStatus::Closed,
            5 => CameraCaptureStatus::Stopped,
            _ => CameraCaptureStatus::Failed,
        }
    }

    /// Stop the device and end its video source. Reports a device stop failure.
    pub fn stop(&mut self) -> Result<(), PeerError> {
        self.source.inner._factory.check_control_call()?;
        let stopped = self.native.as_ref().is_some_and(ffi::close_camera);
        let source_stopped = self.source.close().is_ok();
        (stopped && source_stopped)
            .then_some(())
            .ok_or_else(|| PeerError {
                kind: PeerErrorKind::Operation,
                message: "failed to stop camera or end its video source".into(),
            })
    }
}

#[cfg(feature = "native")]
impl Drop for CameraCapture {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// A sequence-bound caller-fed video source.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::VideoSource>();
/// ```
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<pulsebeam_webrtc_sys::VideoSource>();
/// ```
pub struct VideoSource {
    inner: Rc<SourceInner>,
}

struct SourceInner {
    native: cxx::UniquePtr<ffi::NativeVideoSource>,
    _factory: Rc<FactoryInner>,
}

impl VideoSource {
    pub(crate) fn from_native(
        native: cxx::UniquePtr<ffi::NativeVideoSource>,
        factory: Rc<FactoryInner>,
    ) -> Self {
        Self {
            inner: Rc::new(SourceInner {
                native,
                _factory: factory,
            }),
        }
    }

    pub fn state(&self) -> VideoSourceState {
        VideoSourceState::from_native(ffi::video_source_state(self.native()))
    }

    /// Source-adapter sink requests refused before an unsafe SDK update.
    /// The saturating count includes internal SDK requests, not frame loss or
    /// native source statistics. An unsafe opaque SDK update retires its source
    /// registration, rather than retaining stale track-local expectations.
    pub fn rejected_sink_requests(&self) -> u64 {
        ffi::video_source_rejected_sink_requests(self.native())
    }

    /// Publish a complete immutable SDK frame without discarding metadata or
    /// normalizing its buffer to packed I420.
    pub fn push_native_frame(&self, frame: &crate::RawVideoFrame) -> Result<(), CodecError> {
        if self.state() == VideoSourceState::Ended {
            return Err(CodecError::Released);
        }
        let timestamp = frame.timestamp_us();
        if timestamp == i64::MIN
            || timestamp == i64::MAX
            || ffi::video_source_i420_allocation_size(frame.width() as u32, frame.height() as u32)
                == 0
        {
            return Err(CodecError::InvalidFrame);
        }
        ffi::video_source_push_native_frame(self.native(), frame.native())
            .then_some(())
            .ok_or(CodecError::Released)
    }

    pub fn push_frame(&self, frame: &VideoFrame) -> Result<(), CodecError> {
        if self.state() == VideoSourceState::Ended {
            return Err(CodecError::Released);
        }
        let allocation_size = ffi::video_source_i420_allocation_size(frame.width, frame.height);
        if allocation_size == 0
            || allocation_size != frame.buffer.as_bytes().len()
            || frame.timestamp_us == i64::MIN
            || frame.timestamp_us == i64::MAX
        {
            return Err(CodecError::InvalidFrame);
        }
        ffi::video_source_push_frame(
            self.native(),
            frame.buffer.as_bytes(),
            frame.width,
            frame.height,
            frame.timestamp_us,
            frame.rtp_timestamp,
            frame.rotation as u16,
        )
        .then_some(())
        .ok_or(CodecError::Released)
    }

    pub fn close(&mut self) -> Result<(), CodecError> {
        ffi::close_video_source(self.native())
            .then_some(())
            .ok_or(CodecError::Released)
    }

    /// An owning SDK interface view, not another frame producer.
    pub fn as_view(&self) -> VideoSourceView {
        VideoSourceView {
            inner: Rc::new(VideoSourceViewInner {
                native: ffi::video_source_as_view(self.native()),
                _producer: Some(self.inner.clone()),
                _track: None,
                _factory: self.inner._factory.clone(),
            }),
        }
    }

    pub(crate) fn uses_controlled_execution(&self) -> bool {
        self.inner._factory.uses_controlled_execution()
    }

    pub(crate) fn belongs_to(&self, factory: &Rc<FactoryInner>) -> bool {
        Rc::ptr_eq(&self.inner._factory, factory)
    }

    pub(crate) fn native(&self) -> &ffi::NativeVideoSource {
        self.inner.native.as_ref().expect("validated video source")
    }
}

/// Owning source interface access without another producer or a fabricated
/// stop/push operation. A track view retains that exact track's origin.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::VideoSourceView>();
/// ```
#[derive(Clone)]
pub struct VideoSourceView {
    inner: Rc<VideoSourceViewInner>,
}
struct VideoSourceViewInner {
    native: cxx::UniquePtr<ffi::NativeVideoSourceView>,
    _producer: Option<Rc<SourceInner>>,
    _track: Option<VideoTrack>,
    _factory: Rc<FactoryInner>,
}
impl VideoSourceView {
    /// Compare the actual SDK interface, not logical producer identity.
    /// Different SDK track-source proxies can wrap the same producer.
    pub fn same_interface(&self, other: &Self) -> bool {
        ffi::video_source_views_same_interface(self.native(), other.native())
    }
    pub fn state(&self) -> VideoSourceState {
        VideoSourceState::from_native(ffi::video_source_view_state(self.native()))
    }
    pub fn remote(&self) -> bool {
        ffi::video_source_view_remote(self.native())
    }
    pub fn is_screencast(&self) -> bool {
        ffi::video_source_view_screencast(self.native())
    }
    pub fn needs_denoising(&self) -> Option<bool> {
        match ffi::video_source_view_denoising(self.native()) {
            -1 => None,
            0 => Some(false),
            1 => Some(true),
            value => unreachable!("invalid native denoising value {value}"),
        }
    }
    pub fn supports_encoded_output(&self) -> bool {
        ffi::video_source_view_encoded_output(self.native())
    }
    /// Register on the exact native source's recordable encoded output.
    /// A false SDK SupportsEncodedOutput result is genuinely unsupported.
    /// This does not reserve or replace the receiver transformer slot.
    pub fn attach_encoded_output(&self) -> Result<crate::VideoSourceEncodedSink, PeerError> {
        if !self.supports_encoded_output() {
            return Err(PeerError {
                kind: PeerErrorKind::UnsupportedOperation,
                message: "native video source does not support encoded output".into(),
            });
        }
        let native = ffi::video_source_attach_encoded_output(self.native());
        if native.is_null() {
            return Err(native_error("native source encoded sink creation failed"));
        }
        Ok(crate::VideoSourceEncodedSink::new(native, self.clone()))
    }
    /// Dispatch the native refresh request on the existing owner worker.
    /// A source may implement this as a no-op; this does not promise a frame.
    pub fn request_refresh_frame(&self) {
        ffi::video_source_view_request_refresh(self.native());
    }

    /// Dispatch the native encoded-output keyframe request. The pinned local
    /// push source implements this as a no-op, including when encoded output
    /// is unavailable. No synthetic frame or success indication is produced.
    pub fn generate_key_frame(&self) {
        ffi::video_source_view_generate_key_frame(self.native());
    }

    /// Forward constraints unchanged through the native source interface.
    /// The SDK source proxy dispatches this operation on its worker. Current
    /// push/RTP sources inherit the SDK no-op; no applied limits or sink
    /// callbacks are inferred from this request.
    pub fn process_constraints(&self, constraints: VideoSourceConstraints) {
        ffi::video_source_view_process_constraints(
            self.native(),
            constraints.min_fps.is_some(),
            constraints.min_fps.unwrap_or(0.0),
            constraints.max_fps.is_some(),
            constraints.max_fps.unwrap_or(0.0),
        );
    }

    /// Diagnostics from the actual retained producer, not source-ID matching
    /// or a configuration echo. SDK-owned remote sources have no adapter count.
    pub fn rejected_sink_requests(&self) -> Option<u64> {
        let mut count = 0;
        ffi::video_source_view_rejected_sink_requests(self.native(), &mut count).then_some(count)
    }

    /// Attach directly to this SDK source interface, independently of track
    /// enabled-state rewriting. Aliases share the registration and event queue.
    pub fn attach_sink(&self) -> Result<VideoSourceSink, PeerError> {
        self.attach_sink_with_wants(&crate::VideoSinkWants::default())
    }
    pub fn attach_sink_with_wants(
        &self,
        wants: &crate::VideoSinkWants,
    ) -> Result<VideoSourceSink, PeerError> {
        if self.remote() && self.inner._factory.encoded_video_receive_only() {
            return Err(PeerError {
                kind: PeerErrorKind::UnsupportedOperation,
                message: "factory only receives encoded video; decoded sinks are unavailable"
                    .into(),
            });
        }
        let mut status = 0;
        let native = ffi::video_source_attach_sink(self.native(), &wants.native(), &mut status);
        if native.is_null() {
            return Err(sink_operation_error(status));
        }
        Ok(VideoSourceSink {
            inner: Rc::new(VideoSourceSinkInner {
                native,
                _source: self.clone(),
            }),
        })
    }

    pub fn stats(&self) -> Option<VideoSourceStats> {
        let value = ffi::video_source_view_stats(self.native());
        value.available.then_some(VideoSourceStats {
            input_width: value.input_width,
            input_height: value.input_height,
        })
    }
    /// A fresh collection of SDK queries, not a cached or atomic snapshot.
    pub fn status(&self) -> VideoSourceStatus {
        VideoSourceStatus {
            state: self.state(),
            remote: self.remote(),
            is_screencast: self.is_screencast(),
            needs_denoising: self.needs_denoising(),
            supports_encoded_output: self.supports_encoded_output(),
            stats: self.stats(),
        }
    }
    pub fn observe_changes(
        &self,
        capacity: std::num::NonZeroUsize,
    ) -> Result<VideoObserver, PeerError> {
        VideoObserver::new(
            ffi::video_source_view_observe(self.native(), capacity.get()),
            Some(self.clone()),
            None,
        )
    }
    fn native(&self) -> &ffi::NativeVideoSourceView {
        self.inner
            .native
            .as_ref()
            .expect("owning video source view")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VideoObserverEvent {
    Changed,
}
/// A bounded owning native observer queue. Close unregisters synchronously,
/// retaining accepted notifications and overflow counts for later consumption.
#[derive(Clone)]
pub struct VideoObserver {
    inner: Rc<VideoObserverInner>,
}
struct VideoObserverInner {
    native: cxx::UniquePtr<ffi::NativeVideoObserver>,
    _source: Option<VideoSourceView>,
    _track: Option<VideoTrack>,
}
impl VideoObserver {
    fn new(
        native: cxx::UniquePtr<ffi::NativeVideoObserver>,
        source: Option<VideoSourceView>,
        track: Option<VideoTrack>,
    ) -> Result<Self, PeerError> {
        if native.is_null() {
            return Err(PeerError {
                kind: PeerErrorKind::NativeConstruction,
                message: "native video observer creation failed".into(),
            });
        }
        Ok(Self {
            inner: Rc::new(VideoObserverInner {
                native,
                _source: source,
                _track: track,
            }),
        })
    }
    pub fn try_next_event(&self) -> Option<VideoObserverEvent> {
        ffi::video_observer_take(self.native()).then_some(VideoObserverEvent::Changed)
    }
    pub fn take_dropped_events(&self) -> u64 {
        ffi::video_observer_take_dropped(self.native())
    }
    pub fn close(&self) {
        ffi::video_observer_close(self.native());
    }
    fn native(&self) -> &ffi::NativeVideoObserver {
        self.inner.native.as_ref().expect("owning video observer")
    }
}

/// A sequence-bound local or remote video track.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::VideoTrack>();
/// ```
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<pulsebeam_webrtc_sys::VideoTrack>();
/// ```
#[derive(Clone)]
pub struct VideoTrack {
    inner: Rc<TrackInner>,
}

/// Provenance assigned only by the crate's direct encoded-input adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DirectEncodedVideo {
    H264,
    H264L1T3,
    H264Simulcast,
    Vp8,
    Vp9,
    Av1,
    H265,
}

struct TrackInner {
    native: cxx::UniquePtr<ffi::NativeVideoTrack>,
    _factory: Option<Rc<FactoryInner>>,
    _peer: Option<Rc<PeerInner>>,
    _source: Option<Rc<SourceInner>>,
    encoded_codec: Cell<Option<DirectEncodedVideo>>,
}

impl VideoTrack {
    pub(crate) fn factory_context(&self) -> &Rc<FactoryInner> {
        self.inner
            ._factory
            .as_ref()
            .unwrap_or_else(|| &self.inner._peer.as_ref().unwrap()._factory)
    }
    pub(crate) fn media_track_owner(&self) -> crate::media_track::MediaTrackOwner {
        match &self.inner._peer {
            Some(peer) => {
                let actual = ffi::video_track_reference(self.native());
                for lease in peer.stream_inputs.borrow().values() {
                    if ffi::media_track_same_resource(&actual, lease.track.native()) {
                        return lease.track.source_owner();
                    }
                }
                for lease in peer.sender_tracks.borrow().values() {
                    let candidate = ffi::video_track_reference(lease.track.native());
                    if ffi::media_track_same_resource(&actual, &candidate) {
                        return crate::media_track::MediaTrackOwner::Video {
                            _track: lease.track.clone(),
                        };
                    }
                }
                crate::media_track::MediaTrackOwner::Factory {
                    _factory: peer._factory.clone(),
                }
            }
            None => crate::media_track::MediaTrackOwner::Video {
                _track: self.clone(),
            },
        }
    }
    pub fn as_media_track(&self) -> crate::MediaTrack {
        crate::MediaTrack::from_native(
            ffi::video_track_reference(self.native()),
            self.media_track_owner(),
        )
    }
    pub(crate) fn local(
        native: cxx::UniquePtr<ffi::NativeVideoTrack>,
        factory: Rc<FactoryInner>,
        source: &VideoSource,
    ) -> Self {
        Self {
            inner: Rc::new(TrackInner {
                native,
                _factory: Some(factory),
                _peer: None,
                _source: Some(source.inner.clone()),
                encoded_codec: Cell::new(None),
            }),
        }
    }

    fn remote(native: cxx::UniquePtr<ffi::NativeVideoTrack>, peer: Rc<PeerInner>) -> Self {
        Self {
            inner: Rc::new(TrackInner {
                native,
                _factory: None,
                _peer: Some(peer),
                _source: None,
                encoded_codec: Cell::new(None),
            }),
        }
    }

    pub fn id(&self) -> String {
        ffi::video_track_id(self.native())
    }
    pub fn same_resource(&self, other: &Self) -> bool {
        ffi::video_track_identity(self.native()) == ffi::video_track_identity(other.native())
    }

    pub fn kind(&self) -> String {
        ffi::video_track_kind(self.native())
    }
    pub fn should_receive(&self) -> bool {
        ffi::video_track_should_receive(self.native())
    }
    pub fn set_should_receive(&self, receive: bool) {
        ffi::video_track_set_should_receive(self.native(), receive);
    }
    pub fn content_hint(&self) -> VideoContentHint {
        VideoContentHint::from_native(ffi::video_track_content_hint(self.native()))
    }
    pub fn set_content_hint(&self, hint: VideoContentHint) {
        ffi::video_track_set_content_hint(self.native(), hint.native());
    }
    pub fn status(&self) -> VideoTrackStatus {
        VideoTrackStatus {
            id: self.id(),
            kind: self.kind(),
            state: self.state(),
            enabled: self.enabled(),
            should_receive: self.should_receive(),
            content_hint: self.content_hint(),
        }
    }
    pub fn source(&self) -> Option<VideoSourceView> {
        let native = match self.inner._source.as_ref() {
            Some(source) => ffi::video_track_source_from_producer(
                self.native(),
                self.origin_factory().native(),
                source.native.as_ref().expect("validated video source"),
            ),
            None => ffi::video_track_source(self.native(), self.origin_factory().native()),
        };
        (!native.is_null()).then(|| VideoSourceView {
            inner: Rc::new(VideoSourceViewInner {
                native,
                _producer: None,
                _track: Some(self.clone()),
                _factory: self.origin_factory().clone(),
            }),
        })
    }
    pub fn observe_changes(
        &self,
        capacity: std::num::NonZeroUsize,
    ) -> Result<VideoObserver, PeerError> {
        VideoObserver::new(
            ffi::video_track_observe(
                self.native(),
                self.origin_factory().native(),
                capacity.get(),
            ),
            None,
            Some(self.clone()),
        )
    }
    fn origin_factory(&self) -> &Rc<FactoryInner> {
        self.inner
            ._factory
            .as_ref()
            .unwrap_or_else(|| &self.inner._peer.as_ref().expect("track origin")._factory)
    }

    pub fn enabled(&self) -> bool {
        ffi::video_track_enabled(self.native())
    }

    pub fn set_enabled(&self, enabled: bool) -> bool {
        ffi::video_track_set_enabled(self.native(), enabled)
    }

    pub fn state(&self) -> VideoTrackState {
        match ffi::video_track_state(self.native()) {
            0 => VideoTrackState::Live,
            1 => VideoTrackState::Ended,
            value => unreachable!("native adapter returned invalid video track state {value}"),
        }
    }

    /// Dispatch the track's native refresh request. Frame generation is up to
    /// the source; dispatch does not promise delivery or advance a clock.
    pub fn request_refresh_frame(&self) {
        ffi::video_track_request_refresh(self.native());
    }

    pub fn attach_sink(&self) -> Result<VideoSink, PeerError> {
        self.attach_sink_with_wants(&crate::VideoSinkWants::default())
    }

    pub fn attach_sink_with_wants(
        &self,
        wants: &crate::VideoSinkWants,
    ) -> Result<VideoSink, PeerError> {
        self.attach_sink_impl(wants, None)
    }

    /// Observe actual decoded callbacks synchronously before polling retention
    /// loss. This installs no additional sink or native sink wants. It is not
    /// presentation feedback. Callback panics are exposed by the returned sink.
    pub fn attach_sink_with_observer(
        &self,
        observer: impl crate::DecodedVideoFrameObserver,
    ) -> Result<VideoSink, PeerError> {
        let observer =
            crate::decoded_video_observer::RustDecodedVideoObserver::new(Box::new(observer));
        self.attach_sink_impl(&crate::VideoSinkWants::default(), Some(observer))
    }

    fn attach_sink_impl(
        &self,
        wants: &crate::VideoSinkWants,
        observer: Option<(
            Box<crate::decoded_video_observer::RustDecodedVideoObserver>,
            crate::decoded_video_observer::ObserverStatus,
        )>,
    ) -> Result<VideoSink, PeerError> {
        if self
            .inner
            ._peer
            .as_ref()
            .is_some_and(|peer| peer._factory.encoded_video_receive_only())
        {
            return Err(PeerError {
                kind: PeerErrorKind::UnsupportedOperation,
                message: "factory only receives encoded video; decoded sinks are unavailable"
                    .into(),
            });
        }
        let source = self
            .source()
            .ok_or_else(|| native_error("native video track has no source"))?;
        let mut status = 0;
        let (native, observer_status) = if let Some((observer, observer_status)) = observer {
            (
                ffi::video_track_attach_observed_sink(
                    self.native(),
                    source.native(),
                    &wants.native(),
                    observer,
                    &mut status,
                ),
                Some(observer_status),
            )
        } else {
            (
                ffi::video_track_attach_sink(
                    self.native(),
                    source.native(),
                    &wants.native(),
                    &mut status,
                ),
                None,
            )
        };
        if native.is_null() {
            Err(sink_operation_error(status))
        } else {
            Ok(VideoSink {
                inner: Rc::new(VideoSinkInner {
                    native,
                    observer_status,
                    _track: self.inner.clone(),
                    _source: source,
                }),
            })
        }
    }

    pub(crate) fn mark_direct_encoded(&self, codec: DirectEncodedVideo) {
        self.inner.encoded_codec.set(Some(codec));
    }

    pub(crate) fn is_direct_encoded(&self) -> bool {
        self.inner.encoded_codec.get().is_some()
    }

    pub(crate) fn is_direct_l1t3(&self) -> bool {
        self.inner.encoded_codec.get() == Some(DirectEncodedVideo::H264L1T3)
    }

    pub(crate) fn is_direct_simulcast(&self) -> bool {
        self.inner.encoded_codec.get() == Some(DirectEncodedVideo::H264Simulcast)
    }

    pub(crate) fn is_local_to(&self, factory: &Rc<FactoryInner>) -> bool {
        self.inner
            ._factory
            .as_ref()
            .is_some_and(|owner| Rc::ptr_eq(owner, factory))
    }

    pub(crate) fn native(&self) -> &ffi::NativeVideoTrack {
        self.inner.native.as_ref().expect("validated video track")
    }
}

/// Owned CPU frame with its recipient and wire track identity. Diagnostic
/// observation timing follows the controlled world, without advancing it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReceivedVideoFrame {
    pub peer_id: Option<u64>,
    pub track_id: String,
    pub observed_at: Option<std::time::Duration>,
    pub frame: VideoFrame,
}

/// Native source constraints reported to a raw video sink. Optional zero and
/// absence remain distinct; values are copied without normalization.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VideoSourceConstraints {
    pub min_fps: Option<f64>,
    pub max_fps: Option<f64>,
}

/// Owned non-frame callbacks. Frame output has its separate bounded queue.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum VideoSinkEvent {
    DiscardedFrame,
    ConstraintsChanged(VideoSourceConstraints),
}

impl VideoSinkEvent {
    pub(crate) fn from_native(event: ffi::FfiVideoSinkEvent) -> Option<Self> {
        if !event.valid {
            return None;
        }
        Some(match event.kind {
            0 => Self::DiscardedFrame,
            1 => Self::ConstraintsChanged(VideoSourceConstraints {
                min_fps: event.has_min_fps.then_some(event.min_fps),
                max_fps: event.has_max_fps.then_some(event.max_fps),
            }),
            _ => unreachable!("native video sink returned an invalid callback kind"),
        })
    }
}

#[cfg(test)]
mod video_sink_event_tests {
    use super::*;

    #[test]
    fn control_callbacks_preserve_optional_and_floating_values() {
        for bits in [
            0,
            0x8000000000000000,
            0x7ff0000000000000,
            0xfff0000000000000,
            0x7ff8000000000123,
        ] {
            let event = VideoSinkEvent::from_native(ffi::FfiVideoSinkEvent {
                valid: true,
                kind: 1,
                has_min_fps: true,
                min_fps: f64::from_bits(bits),
                has_max_fps: false,
                max_fps: 0.0,
            });
            let Some(VideoSinkEvent::ConstraintsChanged(value)) = event else {
                panic!("missing constraints callback");
            };
            assert_eq!(value.min_fps.unwrap().to_bits(), bits);
            assert_eq!(value.max_fps, None);
        }
        assert_eq!(
            VideoSinkEvent::from_native(ffi::FfiVideoSinkEvent {
                valid: true,
                kind: 1,
                has_min_fps: false,
                min_fps: 0.0,
                has_max_fps: true,
                max_fps: 0.0,
            }),
            Some(VideoSinkEvent::ConstraintsChanged(VideoSourceConstraints {
                min_fps: None,
                max_fps: Some(0.0),
            })),
        );
    }

    #[test]
    fn discard_and_empty_callbacks_are_distinct() {
        assert_eq!(
            VideoSinkEvent::from_native(ffi::FfiVideoSinkEvent {
                valid: true,
                kind: 0,
                has_min_fps: false,
                min_fps: 0.0,
                has_max_fps: false,
                max_fps: 0.0,
            }),
            Some(VideoSinkEvent::DiscardedFrame),
        );
        assert_eq!(
            VideoSinkEvent::from_native(ffi::FfiVideoSinkEvent {
                valid: false,
                kind: 0,
                has_min_fps: false,
                min_fps: 0.0,
                has_max_fps: false,
                max_fps: 0.0,
            }),
            None,
        );
    }
}

fn sink_operation_error(status: u8) -> PeerError {
    match status {
        1 => PeerError {
            kind: PeerErrorKind::Closed,
            message: "video sink registration is closed".into(),
        },
        2 => PeerError {
            kind: PeerErrorKind::InvalidParameter,
            message:
                "sink alignment exceeds safe SDK signed arithmetic across independent sink removal"
                    .into(),
        },
        _ => native_error("failed to bind native video sink target"),
    }
}
fn registered_sink_wants(native: &ffi::NativeVideoSink) -> Option<crate::VideoSinkWants> {
    let wants = ffi::video_sink_wants(native);
    wants
        .valid
        .then(|| crate::VideoSinkWants::from_native(wants))
}
fn poll_raw_video_frame(native: &ffi::NativeVideoSink) -> Option<VideoFrame> {
    let frame = ffi::video_sink_take_frame(native);
    let frame = frame.as_ref()?;
    Some(
        VideoFrame::i420(
            ffi::native_video_frame_width(frame),
            ffi::native_video_frame_height(frame),
            ffi::native_video_frame_i420(frame),
            ffi::native_video_frame_timestamp_us(frame),
            ffi::native_video_frame_rtp_timestamp(frame),
        )
        .expect("native adapter returned an invalid I420 frame")
        .with_rotation(crate::VideoRotation::from_degrees(
            ffi::native_video_frame_rotation(frame),
        )),
    )
}
macro_rules! raw_video_sink_controls {
    () => {
        /// Read the actual SDK source registration, including track black-frame
        /// rewriting. None means closed or retired, not cached submitted hints.
        pub fn wants(&self) -> Option<crate::VideoSinkWants> {
            registered_sink_wants(self.native())
        }
        /// Forward every native hint. Unsafe alignment is rejected atomically;
        /// this does not promise the source honors adaptation requests.
        pub fn update_wants(&self, wants: &crate::VideoSinkWants) -> Result<(), PeerError> {
            let status = ffi::video_sink_update_wants(self.native(), &wants.native());
            if status == 0 {
                Ok(())
            } else {
                Err(sink_operation_error(status))
            }
        }
        /// Saturating intentional queue loss count, retained after close.
        pub fn dropped_frames(&self) -> u64 {
            ffi::video_sink_dropped_frames(self.native())
        }
        pub fn try_next_event(&self) -> Option<VideoSinkEvent> {
            VideoSinkEvent::from_native(ffi::video_sink_take_event(self.native()))
        }
        pub fn dropped_events(&self) -> u64 {
            ffi::video_sink_dropped_events(self.native())
        }
        /// Shared by aliases. Native removal is a callback barrier and clears
        /// frame retention; queued control notifications remain observable.
        pub fn close(&mut self) -> Result<(), PeerError> {
            if ffi::close_video_sink(self.native()) {
                Ok(())
            } else {
                Err(native_error("failed to detach video sink"))
            }
        }
    };
}

/// A sequence-bound caller-polled video sink.
///
/// At most four frames and 16 MiB of I420 data are retained. When full, the
/// oldest frames are discarded; frames larger than the budget are discarded.
/// Observe cumulative intentional loss with [`Self::dropped_frames`].
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::VideoSink>();
/// ```
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<pulsebeam_webrtc_sys::VideoSink>();
/// ```
#[derive(Clone)]
pub struct VideoSink {
    inner: Rc<VideoSinkInner>,
}
struct VideoSinkInner {
    observer_status: Option<crate::decoded_video_observer::ObserverStatus>,
    // Detach before releasing the exact SDK target and originating factory.
    native: cxx::UniquePtr<ffi::NativeVideoSink>,
    _track: Rc<TrackInner>,
    _source: VideoSourceView,
}
impl VideoSink {
    /// True after a synchronous decoded observer panicked and was retired.
    /// Polling queue loss is separate and remains available as dropped_frames.
    pub fn observer_failed(&self) -> bool {
        self.inner
            .observer_status
            .as_ref()
            .is_some_and(|status| status.failed())
    }

    fn native(&self) -> &ffi::NativeVideoSink {
        self.inner.native.as_ref().expect("validated video sink")
    }
    raw_video_sink_controls!();
    pub fn try_next_received_frame(&self) -> Option<ReceivedVideoFrame> {
        self.try_next_frame().map(|frame| ReceivedVideoFrame {
            peer_id: self
                .inner
                ._track
                ._peer
                .as_ref()
                .and_then(|peer| peer.controlled_id),
            track_id: ffi::video_track_id(
                self.inner
                    ._track
                    .native
                    .as_ref()
                    .expect("validated video track"),
            ),
            observed_at: self
                .inner
                ._track
                ._peer
                .as_ref()
                .and_then(|peer| peer.controlled_time()),
            frame,
        })
    }
    pub fn try_next_frame(&self) -> Option<VideoFrame> {
        if self
            .inner
            ._track
            ._peer
            .as_ref()
            .is_some_and(|peer| peer.closed.get())
        {
            return None;
        }
        if ffi::video_track_state(
            self.inner
                ._track
                .native
                .as_ref()
                .expect("validated video track"),
        ) == 1
        {
            return None;
        }
        poll_raw_video_frame(self.native())
    }
    /// Pop the same callback queue as the legacy value API, retaining the full
    /// SDK frame/buffer graph and acquisition source.
    pub fn try_next_native_frame(&self) -> Option<crate::RawVideoFrame> {
        if self
            .inner
            ._track
            ._peer
            .as_ref()
            .is_some_and(|peer| peer.closed.get())
            || ffi::video_track_state(
                self.inner
                    ._track
                    .native
                    .as_ref()
                    .expect("validated video track"),
            ) == 1
        {
            return None;
        }
        crate::RawVideoFrame::acquired(
            ffi::video_sink_take_native_frame(self.native()),
            self.inner._source.clone(),
        )
    }
}

/// A source-interface registration, not a track registration. Track enabled
/// state does not rewrite its hints. Clone aliases the exact native sink,
/// callback queue, close state and retained source dependency.
#[derive(Clone)]
pub struct VideoSourceSink {
    inner: Rc<VideoSourceSinkInner>,
}
struct VideoSourceSinkInner {
    native: cxx::UniquePtr<ffi::NativeVideoSink>,
    _source: VideoSourceView,
}
impl VideoSourceSink {
    fn native(&self) -> &ffi::NativeVideoSink {
        self.inner.native.as_ref().expect("validated source sink")
    }
    raw_video_sink_controls!();
    pub fn try_next_frame(&self) -> Option<VideoFrame> {
        poll_raw_video_frame(self.native())
    }
    pub fn try_next_native_frame(&self) -> Option<crate::RawVideoFrame> {
        crate::RawVideoFrame::acquired(
            ffi::video_sink_take_native_frame(self.native()),
            self.inner._source.clone(),
        )
    }
}

/// A sequence-bound RTP sender.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::RtpSender>();
/// ```
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<pulsebeam_webrtc_sys::RtpSender>();
/// ```
pub struct RtpSender {
    native: cxx::UniquePtr<ffi::NativeRtpSender>,
    pub(crate) peer: Rc<PeerInner>,
}

/// Full owning native sender-operation errors, distinct from binding failures.
#[derive(Debug)]
pub enum RtpSenderOperationError {
    Binding(PeerError),
    Native(crate::RtcError),
}
impl From<PeerError> for RtpSenderOperationError {
    fn from(error: PeerError) -> Self {
        Self::Binding(error)
    }
}
impl std::fmt::Display for RtpSenderOperationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Binding(error) => std::fmt::Display::fmt(error, f),
            Self::Native(error) => write!(f, "native RTP sender operation failed: {error:?}"),
        }
    }
}
impl std::error::Error for RtpSenderOperationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Binding(error) => Some(error),
            Self::Native(error) => Some(error),
        }
    }
}

/// A mutable subset of an RTP sender encoding. RID is fixed by negotiation.
#[derive(Clone, Debug, PartialEq)]
pub struct RtpSenderEncoding {
    pub rid: String,
    pub active: bool,
    pub max_bitrate_bps: Option<u32>,
    pub max_framerate: Option<f64>,
    pub scale_resolution_down_by: Option<f64>,
    pub scale_resolution_down_to: Option<VideoResolution>,
    pub scalability_mode: Option<String>,
}

/// An owned snapshot. Pass it back to the same sender; stale transactions fail.
#[derive(Clone, Debug, PartialEq)]
pub struct RtpSenderParameters {
    transaction_id: String,
    pub encodings: Vec<RtpSenderEncoding>,
}

/// Exact pinned native media-type alternatives, including non-RTP categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum MediaType {
    Audio = 0,
    Video = 1,
    Data = 2,
    Unsupported = 3,
    Any = 4,
    Other(i32),
}

impl MediaType {
    pub(crate) fn from_native(value: i32) -> Self {
        match value {
            0 => Self::Audio,
            1 => Self::Video,
            2 => Self::Data,
            3 => Self::Unsupported,
            4 => Self::Any,
            value => Self::Other(value),
        }
    }
    pub(crate) fn to_native(self) -> i32 {
        match self {
            Self::Audio => 0,
            Self::Video => 1,
            Self::Data => 2,
            Self::Unsupported => 3,
            Self::Any => 4,
            Self::Other(value) => value,
        }
    }

    /// Forward native conversion for Audio/Video/Data. Unsupported and Any
    /// enter a native assertion branch and are explicitly rejected as `None`.
    pub fn native_name(self) -> Option<String> {
        let mut name = String::new();
        ffi::media_type_to_string(self.to_native(), &mut name).then_some(name)
    }
    /// SDK AbslStringify, with the same native conversion safety boundary.
    pub fn native_string(self) -> Option<String> {
        let mut valid = false;
        let text = ffi::rtp_media_type_string(self.to_native(), &mut valid);
        valid.then_some(text)
    }
}
impl crate::PeerConnection {
    /// A single native owning GetSenders snapshot, including Plan B senders.
    pub fn senders(&self) -> Vec<RtpSender> {
        self.inner.sender_snapshot()
    }
    /// A single native owning GetReceivers snapshot, not a transceiver-derived approximation.
    pub fn receivers(&self) -> Vec<RtpReceiver> {
        let list = ffi::peer_endpoints(self.inner.native());
        (0..ffi::endpoint_list_len(&list, 1))
            .map(|i| {
                RtpReceiver::from_native(ffi::endpoint_list_receiver(&list, i), self.inner.clone())
            })
            .collect()
    }
    pub fn transceivers(&self) -> Result<Vec<RtpTransceiver>, PeerError> {
        let list = ffi::peer_endpoints(self.inner.native());
        if !ffi::endpoint_list_unified(&list) {
            return Err(PeerError {
                kind: PeerErrorKind::UnsupportedOperation,
                message: "GetTransceivers requires Unified Plan semantics".into(),
            });
        }
        Ok((0..ffi::endpoint_list_len(&list, 2))
            .map(|i| {
                RtpTransceiver::from_native(
                    ffi::endpoint_list_transceiver(&list, i),
                    self.inner.clone(),
                )
            })
            .collect())
    }
}
impl PeerInner {
    pub(crate) fn sender_snapshot(self: &Rc<Self>) -> Vec<RtpSender> {
        let list = ffi::peer_endpoints(self.native());
        (0..ffi::endpoint_list_len(&list, 0))
            .map(|i| RtpSender {
                native: ffi::endpoint_list_sender(&list, i),
                peer: self.clone(),
            })
            .collect()
    }
}

impl RtpSender {
    /// SDK creation can change the owning transceiver and request negotiation
    /// even when it returns an error. The selected stock SDK does not implement
    /// the encryption pipeline; errors are returned without fake handles.
    pub fn create_sframe_encrypter(
        &self,
        init: &crate::SframeEncrypterInit,
    ) -> Result<crate::SframeEncrypter, crate::SframeCreateError> {
        crate::SframeEncrypter::from_creation(
            ffi::rtp_sender_create_sframe(self.native(), self.peer.native(), init.native()),
            self.peer.clone(),
        )
    }
    pub fn dtls_transport(&self) -> Option<crate::DtlsTransport> {
        crate::DtlsTransport::from_native(
            ffi::rtp_sender_dtls(self.native(), self.peer.native()),
            self.peer.clone(),
        )
    }

    /// Complete native initial encoding values, copied through the signaling
    /// proxy. Their SDK fields/optionality and comparison are not a partial DTO.
    pub fn init_send_encodings(&self) -> Vec<crate::RtpEncodingParameters> {
        let list = ffi::rtp_sender_initial_encodings(self.native());
        let list = list.as_ref().unwrap();
        (0..ffi::rtp_encoding_list_len(list))
            .map(|index| {
                crate::RtpEncodingParameters::from_native(ffi::rtp_encoding_list_get(list, index))
            })
            .collect()
    }

    /// Primary native SSRC; zero preserves the native not-yet-determined value.
    pub fn ssrc(&self) -> u32 {
        ffi::rtp_sender_ssrc(self.native())
    }

    pub fn media_type(&self) -> MediaType {
        MediaType::from_native(ffi::rtp_sender_media_type(self.native()))
    }

    /// UTF-8 convenience view. Use `stream_id_bytes` for the complete native
    /// string domain, including IDs installed with arbitrary bytes.
    pub fn stream_ids(&self) -> Vec<String> {
        ffi::rtp_sender_stream_ids(self.native())
    }

    /// One actual SDK snapshot, preserving byte strings and native ordering.
    pub fn stream_id_bytes(&self) -> Vec<Vec<u8>> {
        ffi::rtp_sender_stream_id_bytes(self.native())
            .into_iter()
            .map(|id| id.bytes)
            .collect()
    }

    /// Invoke native SetStreams with the supplied IDs/order/duplicates unchanged.
    /// Native de-duplicates by first occurrence, including empty IDs. Read back
    /// native state through `stream_ids`; this void call is not an SDP or
    /// renegotiation acknowledgement.
    /// The native void identity update, preserving opaque bytes without UTF-8 conversion.
    pub fn set_stream_id_bytes(&self, ids: &[Vec<u8>]) {
        let ids: Vec<_> = ids
            .iter()
            .map(|bytes| ffi::FfiRtpByteString {
                bytes: bytes.clone(),
            })
            .collect();
        ffi::rtp_sender_set_stream_id_bytes(self.native(), &ids);
    }
    pub fn set_streams(&self, ids: &[String]) {
        ffi::rtp_sender_set_streams(self.native(), ids);
    }

    /// Shared owning SDK first-packet-sent observer slot with a bounded queue.
    pub fn packet_observer(&self) -> crate::RtpSenderObserver {
        crate::RtpSenderObserver::from_native(
            ffi::rtp_sender_observer(self.native()),
            self.peer.clone(),
        )
    }

    /// Native DTMF access for audio RTP senders. Video senders return `None`.
    /// Repeated getters share one owning handle and one observer registration.
    pub fn dtmf_sender(&self) -> Option<crate::DtmfSender> {
        let native = ffi::rtp_sender_dtmf(self.native(), self.peer.native());
        (!native.is_null()).then(|| crate::DtmfSender::from_native(native, self.peer.clone()))
    }

    pub fn track_reference(&self) -> Option<crate::MediaTrack> {
        let native = ffi::rtp_sender_track_reference(self.native());
        if native.is_null() {
            return None;
        }
        let key = self.resource_key();
        if let Some(lease) = self.peer.stream_inputs.borrow().get(&key)
            && ffi::media_track_same_resource(&native, lease.track.native())
        {
            return Some(lease.track.clone());
        }
        let owner = if let Some(lease) = self.peer.sender_tracks.borrow().get(&key) {
            lease.track.media_track_owner()
        } else if let Some(lease) = self.peer.audio_sender_tracks.borrow().get(&key) {
            lease.track.media_track_owner()
        } else {
            crate::media_track::MediaTrackOwner::Factory {
                _factory: self.peer._factory.clone(),
            }
        };
        Some(crate::MediaTrack::from_native(native, owner))
    }
    pub(crate) fn resource_key(&self) -> usize {
        ffi::rtp_sender_identity(self.native())
    }

    /// Actual SDK interface identity, independent of a track/protocol ID.
    pub fn same_resource(&self, other: &Self) -> bool {
        ffi::rtp_sender_same_resource(self.native(), other.native())
    }

    pub fn id(&self) -> String {
        ffi::rtp_sender_id(self.native())
    }
    pub fn id_bytes(&self) -> Vec<u8> {
        ffi::rtp_sender_id_bytes(self.native())
    }

    pub fn track(&self) -> Option<VideoTrack> {
        let native = ffi::rtp_sender_track(self.native());
        if native.is_null() {
            return None;
        }
        self.peer
            .sender_tracks
            .borrow()
            .get(&self.resource_key())
            .map(|lease| lease.track.clone())
            .or_else(|| Some(VideoTrack::remote(native, self.peer.clone())))
    }

    /// SDK SetTrack for an existing audio or video sender. A false return is
    /// the SDK rejection (for example a kind mismatch or stopped sender).
    /// Accepted tracks retain their original Rust source and factory owners.
    pub fn set_media_track(&self, track: Option<&crate::MediaTrack>) -> Result<bool, PeerError> {
        if self.peer.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer is closed".into(),
            });
        }
        if let Some(track) = track {
            if !Rc::ptr_eq(track.factory(), &self.peer._factory) {
                return Err(PeerError {
                    kind: PeerErrorKind::InvalidParameter,
                    message: "track must belong to this peer factory".into(),
                });
            }
            if let Some(video) = track.local_video() {
                self.validate_video_replacement(video)?;
            }
        }
        let accepted = match track {
            Some(track) => ffi::rtp_sender_set_media_track(self.native(), track.native()),
            None => ffi::rtp_sender_clear_track(self.native()),
        };
        if !accepted {
            return Ok(false);
        }
        let key = self.resource_key();
        self.peer.sender_tracks.borrow_mut().remove(&key);
        self.peer.audio_sender_tracks.borrow_mut().remove(&key);
        match track {
            Some(track) => {
                self.peer
                    .stream_inputs
                    .borrow_mut()
                    .insert(key, crate::peer::SenderTrackLease::new(self, track.clone()));
            }
            None => {
                self.peer.stream_inputs.borrow_mut().remove(&key);
            }
        }
        self.peer.refresh_stream_inputs();
        Ok(true)
    }
    fn validate_video_replacement(&self, track: &VideoTrack) -> Result<(), PeerError> {
        let encodings = self.parameters()?.encodings.len();
        if track.is_direct_encoded()
            && if track.is_direct_simulcast() {
                encodings != 3
            } else {
                encodings > 1
            }
        {
            return Err(PeerError {
                kind: PeerErrorKind::UnsupportedParameter,
                message: "encoded input does not match the sender's encoding layout".into(),
            });
        }
        if !track.is_local_to(&self.peer._factory) || track.state() != VideoTrackState::Live {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "sender replacement requires a live track from the same factory".into(),
            });
        }
        Ok(())
    }

    /// Replace or detach this sender's local video track without replacing the
    /// sender or its negotiated transceiver. Only tracks from the same factory
    /// are accepted. Renegotiate if the remote track identity must change.
    pub fn set_track(&self, track: Option<&VideoTrack>) -> Result<(), PeerError> {
        if self.peer.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer is closed".into(),
            });
        }
        if let Some(track) = track {
            self.validate_video_replacement(track)?;
        }
        let accepted = match track {
            Some(track) => ffi::rtp_sender_set_video_track(self.native(), track.native()),
            None => ffi::rtp_sender_clear_track(self.native()),
        };
        if !accepted {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidState,
                message: "sender rejected the video track change".into(),
            });
        }
        let mut retained = self.peer.sender_tracks.borrow_mut();
        match track {
            Some(track) => {
                retained.insert(
                    self.resource_key(),
                    crate::peer::SenderTrackLease::new(self, track.clone()),
                );
            }
            None => {
                retained.remove(&self.resource_key());
            }
        }
        drop(retained);
        self.peer.refresh_stream_inputs();
        Ok(())
    }

    /// Invoke the actual SDK GenerateKeyFrame with full RID byte strings and
    /// owning native errors. Audio preserves native UnsupportedOperation;
    /// stopped/unconfigured video preserves the SDK's successful no-op before
    /// RID validation. Success is not proof that an encoder produced a frame.
    pub fn generate_keyframe(&self, rids: &[Vec<u8>]) -> Result<(), RtpSenderOperationError> {
        let rids: Vec<_> = rids
            .iter()
            .map(|rid| ffi::FfiRtpByteString { bytes: rid.clone() })
            .collect();
        let native = ffi::rtp_sender_generate_keyframe(self.native(), self.peer.native(), &rids);
        if native.is_null() {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidState,
                message: "sender peer is unavailable".into(),
            }
            .into());
        }
        let error = crate::RtcError::from_native(native);
        if error.is_ok() {
            Ok(())
        } else {
            Err(RtpSenderOperationError::Native(error))
        }
    }

    /// Ask the active video sender to produce a keyframe for the listed RIDs.
    /// An empty slice targets all configured encodings. Successful submission
    /// is not proof that a frame was produced: the sender must have a live
    /// sending media channel and an encoder that honors keyframe requests.
    pub fn request_keyframe(&self, rids: &[String]) -> Result<(), PeerError> {
        if self.peer.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer is closed".into(),
            });
        }
        if self.track().is_none() {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidState,
                message: "video sender has no track".into(),
            });
        }
        let mut error_type = 0;
        let mut message = String::new();
        ffi::rtp_sender_request_keyframe(
            self.native(),
            self.peer.native(),
            rids,
            &mut error_type,
            &mut message,
        )
        .then_some(())
        .ok_or_else(|| PeerError {
            kind: error_kind(error_type),
            message,
        })
    }

    pub(crate) fn has_negotiated_dependency_descriptor(&self) -> bool {
        const URI: &str = "https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension";
        let list = ffi::peer_video_transceivers(self.peer.native());
        let Some(list) = list.as_ref() else {
            return false;
        };
        let sender_id = self.id();
        for index in 0..ffi::transceiver_list_len(list) {
            let transceiver = ffi::transceiver_list_at(list, index);
            let Some(transceiver) = transceiver.as_ref() else {
                continue;
            };
            let sender = ffi::rtp_transceiver_sender(transceiver);
            if sender
                .as_ref()
                .is_some_and(|sender| ffi::rtp_sender_id(sender) == sender_id)
            {
                return ffi::rtp_transceiver_header_extensions(transceiver, true)
                    .into_iter()
                    .any(|extension| {
                        extension.uri == URI
                            && (extension.direction
                                == RtpHeaderExtensionDirection::SendReceive as u8
                                || extension.direction
                                    == RtpHeaderExtensionDirection::SendOnly as u8)
                    });
            }
        }
        false
    }

    /// Complete owned SDK GetParameters snapshot, via the signaling proxy.
    /// Every call starts a new SDK transaction and invalidates previous snapshots
    /// for SetParameters. Children retain no sender/factory origin.
    pub fn get_parameters(&self) -> crate::RtpParameters {
        crate::RtpParameters::from_native(ffi::rtp_sender_parameters_value(self.native()))
    }

    /// Legacy partial overlay snapshot. This is not the full SDK value API.
    pub fn parameters(&self) -> Result<RtpSenderParameters, PeerError> {
        let mut snapshot = ffi::FfiSenderParameters {
            transaction_id: String::new(),
            encodings: Vec::new(),
        };
        if !ffi::rtp_sender_get_parameters(self.native(), &mut snapshot) {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidState,
                message: "RTP sender parameters are unavailable".into(),
            });
        }
        Ok(RtpSenderParameters {
            transaction_id: snapshot.transaction_id,
            encodings: snapshot
                .encodings
                .into_iter()
                .map(|encoding| RtpSenderEncoding {
                    rid: encoding.rid,
                    active: encoding.active,
                    max_bitrate_bps: encoding
                        .max_bitrate_bps
                        .try_into()
                        .ok()
                        .filter(|_| encoding.has_max_bitrate),
                    max_framerate: encoding.has_max_framerate.then_some(encoding.max_framerate),
                    scale_resolution_down_by: encoding.has_scale_by.then_some(encoding.scale_by),
                    scale_resolution_down_to: encoding.has_scale_to.then_some(VideoResolution {
                        width: encoding.scale_to_width as u32,
                        height: encoding.scale_to_height as u32,
                    }),
                    scalability_mode: encoding
                        .has_scalability_mode
                        .then_some(encoding.scalability_mode),
                })
                .collect(),
        })
    }

    /// Updates supported fields using the latest parameters snapshot. Unexposed
    /// fields are preserved, and native validation rejects unsupported modes.
    pub fn set_parameters(&self, parameters: RtpSenderParameters) -> Result<(), PeerError> {
        if self.track().is_some_and(|track| track.is_direct_encoded())
            && (if self
                .track()
                .is_some_and(|track| track.is_direct_simulcast())
            {
                parameters.encodings.len() != 3
            } else {
                parameters.encodings.len() > 1
            } || parameters.encodings.iter().any(|encoding| {
                (encoding.scalability_mode.is_some()
                    && !(self.track().is_some_and(|track| track.is_direct_l1t3())
                        && encoding.scalability_mode.as_deref() == Some("L1T3")))
                    || (!self
                        .track()
                        .is_some_and(|track| track.is_direct_simulcast())
                        && (encoding.scale_resolution_down_to.is_some()
                            || encoding
                                .scale_resolution_down_by
                                .is_some_and(|scale| scale != 1.0)))
            }))
        {
            return Err(PeerError {
                kind: PeerErrorKind::UnsupportedParameter,
                message: "direct encoded input does not support scaling, simulcast, or this scalability mode".into(),
            });
        }
        if self.track().is_some_and(|track| track.is_direct_l1t3())
            && parameters
                .encodings
                .iter()
                .any(|encoding| encoding.scalability_mode.as_deref() == Some("L1T3"))
            && !self.has_negotiated_dependency_descriptor()
        {
            return Err(PeerError {
                kind: PeerErrorKind::UnsupportedParameter,
                message: "direct H264 L1T3 requires a negotiated native dependency descriptor"
                    .into(),
            });
        }
        let invalid = |message: &str| PeerError {
            kind: PeerErrorKind::InvalidParameter,
            message: message.into(),
        };
        let mut encodings = Vec::with_capacity(parameters.encodings.len());
        for encoding in parameters.encodings {
            let max_bitrate = encoding
                .max_bitrate_bps
                .map(i32::try_from)
                .transpose()
                .map_err(|_| invalid("maximum bitrate exceeds i32 range"))?;
            if max_bitrate == Some(0) {
                return Err(invalid("maximum bitrate must be positive"));
            }
            if encoding
                .max_framerate
                .is_some_and(|v| !v.is_finite() || v < 0.0 || v >= (i32::MAX as f64) + 1.0)
            {
                // SDK video encoder configuration converts this double to int.
                // Truncation is representable below the exclusive MAX + 1 bound.
                return Err(invalid(
                    "maximum framerate is outside safe SDK integer conversion",
                ));
            }
            if encoding
                .scale_resolution_down_by
                .is_some_and(|v| !v.is_finite() || v < 1.0)
            {
                return Err(invalid("resolution scale must be finite and at least one"));
            }
            if encoding.scale_resolution_down_to.is_some_and(|r| {
                r.width == 0
                    || r.height == 0
                    || r.width > i32::MAX as u32
                    || r.height > i32::MAX as u32
                    || u64::from(r.width) * u64::from(r.height) > i32::MAX as u64
            }) {
                // Downstream layer selection calls SDK Resolution::PixelCount,
                // whose signed-int product must remain representable.
                return Err(invalid(
                    "target resolution is outside safe SDK PixelCount consumption",
                ));
            }
            if encoding.scalability_mode.as_deref() == Some("") {
                return Err(invalid("scalability mode must not be empty"));
            }
            let target = encoding.scale_resolution_down_to;
            encodings.push(ffi::FfiSenderEncoding {
                rid: encoding.rid,
                active: encoding.active,
                has_max_bitrate: max_bitrate.is_some(),
                max_bitrate_bps: max_bitrate.unwrap_or_default(),
                has_max_framerate: encoding.max_framerate.is_some(),
                max_framerate: encoding.max_framerate.unwrap_or_default(),
                has_scale_by: encoding.scale_resolution_down_by.is_some(),
                scale_by: encoding.scale_resolution_down_by.unwrap_or_default(),
                has_scale_to: target.is_some(),
                scale_to_width: target.map(|r| r.width as i32).unwrap_or_default(),
                scale_to_height: target.map(|r| r.height as i32).unwrap_or_default(),
                has_scalability_mode: encoding.scalability_mode.is_some(),
                scalability_mode: encoding.scalability_mode.unwrap_or_default(),
            });
        }
        let mut error_type = 0;
        let mut message = String::new();
        ffi::rtp_sender_set_parameters(
            self.native(),
            &ffi::FfiSenderParameters {
                transaction_id: parameters.transaction_id,
                encodings,
            },
            &mut error_type,
            &mut message,
        )
        .then_some(())
        .ok_or_else(|| PeerError {
            kind: error_kind(error_type),
            message,
        })
    }

    /// Select caller transformation inside the host's stable SDK adapter.
    /// None retires caller requests, not the SDK adapter or required metadata.
    /// The pinned SDK cannot reliably detach/reinitialize native delegates.
    pub fn set_frame_transformer(
        &self,
        transformer: Option<&crate::FrameTransformer>,
    ) -> Result<(), PeerError> {
        if self.peer.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer is closed".into(),
            });
        }
        if transformer.is_some_and(crate::FrameTransformer::is_closed) {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "caller transformer is retired".into(),
            });
        }
        let value = transformer.map_or(std::ptr::null(), |value| value.native() as *const _);
        // SAFETY: synchronous optional borrow; the host retains shared ownership.
        unsafe {
            ffi::rtp_sender_select_frame_transformer(self.peer.native(), self.native(), value)
        }
        .then_some(())
        .ok_or_else(|| native_error("sender transformer host does not belong to this peer"))
    }
    #[deprecated(note = "Use set_frame_transformer")]
    pub fn set_encoder_to_packetizer_frame_transformer(
        &self,
        transformer: Option<&crate::FrameTransformer>,
    ) -> Result<(), PeerError> {
        self.set_frame_transformer(transformer)
    }

    /// Install or clear the SDK's refcounted frame encryptor. This is additional
    /// frame encryption, not a replacement for native SRTP authentication.
    /// Install the actual ref-counted native sender selector. Aliases share
    /// the same provider; the SDK owns installation and encoder switching.
    pub fn set_encoder_selector(&self, selector: Option<&crate::VideoEncoderSelector>) {
        let pointer = selector.map_or(std::ptr::null(), |v| v.native() as *const _);
        // SAFETY: the SDK retains its own ref-counted copy during this call.
        unsafe {
            ffi::rtp_sender_set_encoder_selector(self.native(), pointer);
        }
    }
    /// Deprecated unique-ownership SDK overload, distinct from shared aliasing.
    pub fn set_owned_encoder_selector(&self, selector: Option<crate::OwnedVideoEncoderSelector>) {
        ffi::rtp_sender_set_owned_encoder_selector(
            self.native(),
            selector.map_or_else(
                cxx::UniquePtr::null,
                crate::OwnedVideoEncoderSelector::into_native,
            ),
        );
    }
    pub fn set_frame_encryptor(&self, encryptor: Option<&crate::FrameEncryptor>) {
        let value = encryptor
            .map(|value| value.native() as *const _)
            .unwrap_or(std::ptr::null());
        // SAFETY: the optional pointer is borrowed for this synchronous proxy
        // call; the SDK retains its own reference before the borrow ends.
        unsafe { ffi::rtp_sender_set_frame_encryptor(self.native(), value) };
    }
    pub fn frame_encryptor(&self) -> Option<crate::FrameEncryptor> {
        crate::FrameEncryptor::from_native(ffi::rtp_sender_frame_encryptor(self.native()))
    }

    pub(crate) fn native(&self) -> &ffi::NativeRtpSender {
        self.native.as_ref().expect("validated RTP sender")
    }
}

/// A sequence-bound RTP receiver.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::RtpReceiver>();
/// ```
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<pulsebeam_webrtc_sys::RtpReceiver>();
/// ```
pub struct RtpReceiver {
    native: cxx::UniquePtr<ffi::NativeRtpReceiver>,
    pub(crate) peer: Rc<PeerInner>,
}

impl fmt::Debug for RtpReceiver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RtpReceiver")
            .field("id", &self.id())
            .finish()
    }
}

/// A binding lifecycle failure or an unsafe receiver-delay consumption.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RtpReceiverOperationError {
    Binding(PeerError),
    /// Native jitter-delay conversion CHECKs when the cached delay is NaN.
    UnsafeDelayNaN,
}
impl fmt::Display for RtpReceiverOperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Binding(error) => error.fmt(f),
            Self::UnsafeDelayNaN => {
                f.write_str("native jitter-buffer delay conversion cannot consume NaN")
            }
        }
    }
}
impl std::error::Error for RtpReceiverOperationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Binding(error) => Some(error),
            Self::UnsafeDelayNaN => None,
        }
    }
}
impl From<PeerError> for RtpReceiverOperationError {
    fn from(error: PeerError) -> Self {
        Self::Binding(error)
    }
}

impl RtpReceiver {
    pub fn track_reference(&self) -> Option<crate::MediaTrack> {
        let native = ffi::rtp_receiver_track_reference(self.native());
        (!native.is_null()).then(|| {
            crate::MediaTrack::from_native(
                native,
                crate::media_track::MediaTrackOwner::Factory {
                    _factory: self.peer._factory.clone(),
                },
            )
        })
    }
    pub fn same_resource(&self, other: &Self) -> bool {
        ffi::rtp_receiver_same_resource(self.native(), other.native())
    }

    /// Preserve native error and transceiver/negotiation side effects. SFrame
    /// key management is distinct from the legacy frame decryptor provider.
    pub fn create_sframe_decrypter(
        &self,
        suite: crate::SframeCipherSuite,
    ) -> Result<crate::SframeDecrypter, crate::SframeCreateError> {
        crate::SframeDecrypter::from_creation(
            ffi::rtp_receiver_create_sframe(self.native(), self.peer.native(), suite.raw()),
            self.peer.clone(),
        )
    }
    /// Select caller transformation before the receiver's encoded collector.
    /// Retiring the caller preserves the stable native delegate and collector.
    pub fn set_frame_transformer(
        &self,
        transformer: Option<&crate::FrameTransformer>,
    ) -> Result<(), PeerError> {
        if self.peer.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer is closed".into(),
            });
        }
        if transformer.is_some_and(crate::FrameTransformer::is_closed) {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidParameter,
                message: "caller transformer is retired".into(),
            });
        }
        let value = transformer.map_or(std::ptr::null(), |value| value.native() as *const _);
        // SAFETY: actual native receiver identity is checked; ownership is retained.
        unsafe {
            ffi::rtp_receiver_select_frame_transformer(self.peer.native(), self.native(), value)
        }
        .then_some(())
        .ok_or_else(|| native_error("receiver transformer host does not belong to this peer"))
    }
    #[deprecated(note = "Use set_frame_transformer")]
    pub fn set_depacketizer_to_decoder_frame_transformer(
        &self,
        transformer: Option<&crate::FrameTransformer>,
    ) -> Result<(), PeerError> {
        self.set_frame_transformer(transformer)
    }

    /// Construct a decryptor bound permanently to this native receiver.
    pub fn create_frame_decryptor(
        &self,
        provider: impl crate::FrameDecryptorProvider,
    ) -> crate::FrameDecryptor {
        crate::FrameDecryptor::new(self.native(), self.peer.clone(), provider)
    }
    pub fn set_frame_decryptor(
        &self,
        decryptor: Option<&crate::FrameDecryptor>,
    ) -> Result<(), crate::FrameDecryptorInstallError> {
        let value = decryptor
            .map(|value| value.native() as *const _)
            .unwrap_or(std::ptr::null());
        // SAFETY: the native wrapper checks exact receiver identity before
        // installation. The SDK retains its reference during the proxy call.
        unsafe { ffi::rtp_receiver_set_frame_decryptor(self.native(), value) }
            .then_some(())
            .ok_or(crate::FrameDecryptorInstallError::WrongReceiver)
    }
    pub fn frame_decryptor(&self) -> Option<crate::FrameDecryptor> {
        crate::FrameDecryptor::from_native(
            ffi::rtp_receiver_frame_decryptor(self.native()),
            self.peer.clone(),
        )
    }

    /// Copies the SDK's current SSRC and CSRC observations, including native
    /// timestamps and all available header extensions. No track/source join,
    /// received VLA, or passive keyframe notification is inferred.
    pub fn get_sources(&self) -> Vec<crate::RtpSource> {
        let sources = ffi::rtp_receiver_sources(self.native());
        let sources = sources.as_ref().expect("native RTP source snapshot");
        (0..ffi::rtp_source_list_len(sources))
            .map(|index| crate::RtpSource::from_native(ffi::rtp_source_list_get(sources, index)))
            .collect()
    }

    /// Copies the complete SDK receiver parameters through its worker proxy.
    /// The result owns its metadata independently of this receiver and peer.
    /// Unlike the sender getter, this does not start a sender transaction.
    pub fn get_parameters(&self) -> crate::RtpParameters {
        crate::RtpParameters::from_native(ffi::rtp_receiver_parameters_value(self.native()))
    }

    /// Calls the legacy SDK receiver setter. The pinned stock implementation
    /// does not support changes and returns false, including for an unchanged
    /// snapshot. No synthetic success or sender operation is substituted.
    pub fn set_parameters(&self, parameters: &crate::RtpParameters) -> bool {
        ffi::rtp_receiver_set_parameters_value(self.native(), parameters.native())
    }

    /// Sets the native minimum playout delay in seconds. None restores the
    /// native default; Some(0.0) remains an explicit zero. The SDK performs its
    /// own saturation and 0..=10000-ms clamping, including for infinities and
    /// negative values. Only NaN, which triggers a native CHECK, is rejected
    /// before it can be cached for later stream start.
    pub fn set_jitter_buffer_minimum_delay(
        &self,
        seconds: Option<f64>,
    ) -> Result<(), RtpReceiverOperationError> {
        ffi::rtp_receiver_set_jitter_buffer_minimum_delay(
            self.native(),
            seconds.is_some(),
            seconds.unwrap_or(0.0),
        )
        .then_some(())
        .ok_or(RtpReceiverOperationError::UnsafeDelayNaN)
    }

    /// Shared owning SDK first-packet/receptive-change observer slot.
    /// First-packet notification can fan out to all same-kind receivers and
    /// does not establish delivery to this receiver's SSRC.
    pub fn packet_observer(&self) -> crate::RtpReceiverObserver {
        crate::RtpReceiverObserver::from_native(
            ffi::rtp_receiver_observer(self.native()),
            self.peer.clone(),
        )
    }

    pub fn dtls_transport(&self) -> Option<crate::DtlsTransport> {
        crate::DtlsTransport::from_native(
            ffi::rtp_receiver_dtls(self.native(), self.peer.native()),
            self.peer.clone(),
        )
    }

    pub(crate) fn from_native(
        native: cxx::UniquePtr<ffi::NativeRtpReceiver>,
        peer: Rc<PeerInner>,
    ) -> Self {
        Self { native, peer }
    }

    pub fn id(&self) -> String {
        ffi::rtp_receiver_id(self.native())
    }
    pub fn id_bytes(&self) -> Vec<u8> {
        ffi::rtp_receiver_id_bytes(self.native())
    }

    pub fn media_type(&self) -> MediaType {
        MediaType::from_native(ffi::rtp_receiver_media_type(self.native()))
    }

    pub fn stream_ids(&self) -> Vec<String> {
        ffi::rtp_receiver_stream_ids(self.native())
    }
    pub fn stream_id_bytes(&self) -> Vec<Vec<u8>> {
        ffi::rtp_receiver_stream_id_bytes(self.native())
            .into_iter()
            .map(|id| id.bytes)
            .collect()
    }
    pub fn streams(&self) -> Vec<crate::MediaStream> {
        let list = ffi::rtp_receiver_streams(self.native());
        (0..ffi::media_streams_len(&list))
            .map(|i| {
                crate::MediaStream::from_native(
                    ffi::media_streams_get(&list, i),
                    self.peer._factory.clone(),
                )
            })
            .collect()
    }

    pub fn track(&self) -> Option<VideoTrack> {
        let native = ffi::rtp_receiver_track(self.native());
        (!native.is_null()).then(|| VideoTrack::remote(native, self.peer.clone()))
    }

    /// Owning native audio track, distinct from the video-only `track` view.
    pub fn audio_track(&self) -> Option<crate::AudioTrack> {
        let native = ffi::rtp_receiver_audio_track(self.peer.native(), self.native());
        (!native.is_null()).then(|| crate::AudioTrack::received(native, self.peer.clone()))
    }

    /// Attach a bounded decoded PCM sink to a receiver on this peer.
    /// Only one audio sink may be active; close it before selecting either mode
    /// again. Closing an encoded sink restores the native decoder first.
    pub fn attach_audio_sink(&self) -> Result<crate::AudioSink, PeerError> {
        if self.peer.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer is closed".into(),
            });
        }
        let native = ffi::rtp_receiver_attach_audio_sink(self.peer.native(), self.native());
        if native.is_null() {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidState,
                message: "receiver is foreign, ended, not audio, or already has an audio sink"
                    .into(),
            });
        }
        Ok(crate::AudioSink::from_native(
            native,
            self.peer.clone(),
            self.id(),
        ))
    }

    /// Receive encoded Opus without decoding. Select this instead of a decoded
    /// audio sink before media arrives. Only one audio sink may be active.
    /// Closing permits reattachment or an explicit mode change, but restores
    /// native decoding in the interval. For opaque packets, stop reception
    /// before closing and resume only after attaching another encoded sink.
    pub fn attach_encoded_audio_sink(&self) -> Result<crate::EncodedAudioSink, PeerError> {
        if self.peer.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer is closed".into(),
            });
        }
        let native = ffi::rtp_receiver_attach_encoded_audio_sink(self.peer.native(), self.native());
        if native.is_null() {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidState,
                message:
                    "receiver is foreign, not negotiated for Opus, or already has an audio sink"
                        .into(),
            });
        }
        Ok(crate::EncodedAudioSink::from_native(
            native,
            self.peer.clone(),
        ))
    }

    /// Request a fresh remote video keyframe through the negotiated receiver.
    /// Success means the request was submitted, not that the sender delivered
    /// a frame. A foreign, ended or non-video receiver is rejected.
    pub fn request_keyframe(&self) -> Result<(), PeerError> {
        if self.peer.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer is closed".into(),
            });
        }
        ffi::rtp_receiver_request_keyframe(self.peer.native(), self.native())
            .then_some(())
            .ok_or_else(|| PeerError {
                kind: PeerErrorKind::InvalidState,
                message: "receiver is foreign, ended or not a remote video receiver".into(),
            })
    }

    /// Consume depacketized encoded video before decoding. Only one sink can
    /// ever be attached to each receiver during a peer's lifetime. Closing
    /// the sink restores pass-through decoding; it cannot be reattached.
    pub fn attach_encoded_sink(&self) -> Result<EncodedVideoSink, PeerError> {
        self.attach_encoded_sink_with_ordering(false, None)
    }

    /// Observe encoded video after native reference finding and frame-buffer
    /// ordering. Original frames continue to the configured decoder; this sink
    /// does not suppress decoding or manufacture decoded pixels. A decoder
    /// accepting the negotiated format is required. The same receiver-lifetime
    /// attachment limit applies across both encoded sink modes.
    pub fn attach_ordered_encoded_sink(&self) -> Result<EncodedVideoSink, PeerError> {
        self.attach_encoded_sink_with_ordering(true, None)
    }

    /// Observe native ordered encoded callbacks before independent polling
    /// retention. The configured decoder still receives the original frames.
    pub fn attach_ordered_encoded_sink_with_observer(
        &self,
        observer: impl crate::EncodedVideoFrameObserver,
    ) -> Result<EncodedVideoSink, PeerError> {
        let observer =
            crate::encoded_video_observer::RustEncodedVideoObserver::new(Box::new(observer));
        self.attach_encoded_sink_with_ordering(true, Some(observer))
    }

    fn attach_encoded_sink_with_ordering(
        &self,
        ordered: bool,
        observer: Option<(
            Box<crate::encoded_video_observer::RustEncodedVideoObserver>,
            crate::encoded_video_observer::ObserverStatus,
        )>,
    ) -> Result<EncodedVideoSink, PeerError> {
        if self.peer.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer is closed".into(),
            });
        }
        let (native, observer_status) = if let Some((observer, status)) = observer {
            (
                ffi::rtp_receiver_attach_observed_encoded_video_sink(
                    self.peer.native(),
                    self.native(),
                    observer,
                ),
                Some(status),
            )
        } else {
            (
                ffi::rtp_receiver_attach_encoded_video_sink(
                    self.peer.native(),
                    self.native(),
                    ordered,
                ),
                None,
            )
        };
        if native.is_null() {
            return Err(PeerError {
                kind: PeerErrorKind::InvalidState,
                message: "receiver is foreign or already has an encoded sink".into(),
            });
        }
        Ok(EncodedVideoSink {
            native,
            observer_status,
            _peer: self.peer.clone(),
        })
    }

    pub(crate) fn native(&self) -> &ffi::NativeRtpReceiver {
        self.native.as_ref().expect("validated RTP receiver")
    }
}

/// An encoded access unit after RTP reassembly. Ordered observation is released
/// at native decode scheduling; raw interception precedes reference finding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EncodedReceivedVideoFrame {
    pub data: Vec<u8>,
    pub mime_type: String,
    pub rtp_timestamp: u32,
    pub ssrc: u32,
    pub payload_type: u8,
    pub key_frame: bool,
    pub rid: Option<String>,
    /// Remote capture clock microseconds, when the absolute capture-time
    /// header extension provides it. Not a local wall-clock timestamp.
    /// Native infinity sentinels are preserved as i64::MIN/i64::MAX.
    pub capture_time_us: Option<i64>,
    /// Local receive clock microseconds when supplied by upstream.
    /// Native infinity sentinels are preserved as i64::MIN/i64::MAX.
    pub receive_time_us: Option<i64>,
    /// Dependency descriptor frame id, if present for this access unit.
    pub frame_id: Option<i64>,
    pub spatial_index: Option<i32>,
    pub temporal_index: Option<i32>,
    pub dependencies: Vec<i64>,
    pub decode_target_indications: Vec<DecodeTargetIndication>,
}
/// Decode target indication from the negotiated dependency descriptor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DecodeTargetIndication {
    NotPresent,
    Discardable,
    Switch,
    Required,
}

/// A bounded receive-only queue, exclusive to one receiver. A full queue
/// discards the oldest frame. A frame too large for the 4 MiB cap is dropped.
/// Raw interception suppresses decoding; ordered observation leaves it active.
pub struct EncodedVideoSink {
    native: cxx::UniquePtr<ffi::NativeEncodedVideoSink>,
    observer_status: Option<crate::encoded_video_observer::ObserverStatus>,
    _peer: Rc<PeerInner>,
}

impl EncodedReceivedVideoFrame {
    pub(crate) fn from_native(frame: ffi::FfiEncodedVideoFrame) -> Option<Self> {
        frame.available.then_some(Self {
            data: frame.data,
            mime_type: frame.mime_type,
            rtp_timestamp: frame.rtp_timestamp,
            ssrc: frame.ssrc,
            payload_type: frame.payload_type,
            key_frame: frame.key_frame,
            rid: frame.has_rid.then_some(frame.rid),
            capture_time_us: frame.has_capture_time.then_some(frame.capture_time_us),
            receive_time_us: frame.has_receive_time.then_some(frame.receive_time_us),
            frame_id: frame.has_frame_id.then_some(frame.frame_id),
            spatial_index: frame.has_frame_id.then_some(frame.spatial_index),
            temporal_index: frame.has_frame_id.then_some(frame.temporal_index),
            dependencies: frame.dependencies,
            decode_target_indications: frame
                .decode_target_indications
                .into_iter()
                .map(|value| match value {
                    0 => DecodeTargetIndication::NotPresent,
                    1 => DecodeTargetIndication::Discardable,
                    2 => DecodeTargetIndication::Switch,
                    3 => DecodeTargetIndication::Required,
                    _ => unreachable!("upstream emitted invalid decode target indication"),
                })
                .collect(),
        })
    }
}

impl EncodedVideoSink {
    pub fn try_next_frame(&self) -> Option<EncodedReceivedVideoFrame> {
        EncodedReceivedVideoFrame::from_native(ffi::encoded_video_sink_take_frame(
            self.native.as_ref().expect("validated sink"),
        ))
    }

    /// True after a synchronous encoded observer panicked and was retired.
    pub fn observer_failed(&self) -> bool {
        self.observer_status
            .as_ref()
            .is_some_and(|status| status.failed())
    }

    /// Saturating count of dropped access units, including oversize frames.
    pub fn dropped_frames(&self) -> u64 {
        ffi::encoded_video_sink_dropped_frames(self.native.as_ref().expect("validated sink"))
    }

    pub fn close(&mut self) {
        ffi::close_encoded_video_sink(self.native.as_ref().expect("validated sink"));
    }
}

/// A sequence-bound RTP transceiver.
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<pulsebeam_webrtc_sys::RtpTransceiver>();
/// ```
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<pulsebeam_webrtc_sys::RtpTransceiver>();
/// ```
pub struct RtpTransceiver {
    native: cxx::UniquePtr<ffi::NativeRtpTransceiver>,
    peer: Rc<PeerInner>,
}

impl fmt::Debug for RtpTransceiver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RtpTransceiver")
            .field("direction", &self.direction_value())
            .field("current_direction", &self.current_direction_value())
            .field("stopped", &self.stopped())
            .finish()
    }
}

impl RtpTransceiver {
    pub fn same_resource(&self, other: &Self) -> bool {
        ffi::rtp_transceiver_same_resource(self.native(), other.native())
    }

    pub(crate) fn from_native(
        native: cxx::UniquePtr<ffi::NativeRtpTransceiver>,
        peer: Rc<PeerInner>,
    ) -> Self {
        Self { native, peer }
    }

    pub fn sender(&self) -> RtpSender {
        RtpSender {
            native: ffi::rtp_transceiver_sender(self.native()),
            peer: self.peer.clone(),
        }
    }

    pub fn receiver(&self) -> RtpReceiver {
        RtpReceiver::from_native(
            ffi::rtp_transceiver_receiver(self.native()),
            self.peer.clone(),
        )
    }

    pub fn media_type(&self) -> MediaType {
        MediaType::from_native(ffi::rtp_transceiver_media_type(self.native()))
    }

    pub fn stopping(&self) -> bool {
        ffi::rtp_transceiver_stopping(self.native())
    }

    pub fn receptive(&self) -> bool {
        ffi::rtp_transceiver_receptive(self.native())
    }

    /// Public SDK Chromium-oriented fired-direction metadata, without
    /// conflating absence with any raw signed enum value.
    pub fn fired_direction(&self) -> Option<crate::RtpTransceiverDirectionValue> {
        let mut raw = 0;
        ffi::rtp_transceiver_fired_direction(self.native(), &mut raw)
            .then(|| crate::RtpTransceiverDirectionValue::from_raw(raw))
    }

    /// The SDK's optional SFrame state. None is not replaced with false.
    pub fn sframe_enabled(&self) -> Option<bool> {
        let mut enabled = false;
        ffi::rtp_transceiver_sframe_enabled(self.native(), &mut enabled).then_some(enabled)
    }

    /// Full signed SDK direction metadata, including Stopped and unknown values.
    /// This does not change the closed legacy set_direction consumer contract.
    pub fn direction_value(&self) -> crate::RtpTransceiverDirectionValue {
        crate::RtpTransceiverDirectionValue::from_raw(ffi::rtp_transceiver_direction_value(
            self.native(),
        ))
    }

    /// Exact SDK optional current direction. Native absence is separate from
    /// every raw enum value, including -1; no narrowed integer sentinel is used.
    pub fn current_direction_value(&self) -> Option<crate::RtpTransceiverDirectionValue> {
        let mut raw = 0;
        ffi::rtp_transceiver_current_direction_value(self.native(), &mut raw)
            .then(|| crate::RtpTransceiverDirectionValue::from_raw(raw))
    }

    /// Call SDK SetDirectionWithError with full known enum values and retain
    /// its owning error. Stopped is forwarded for native policy rejection.
    /// Unknown raw directions remain metadata, but are rejected here because
    /// later native offer codec selection CHECKs after the SDK accepts them.
    pub fn set_direction_value(
        &self,
        direction: crate::RtpTransceiverDirectionValue,
    ) -> Result<(), crate::RtpTransceiverOperationError> {
        self.check_capability_peer_open()
            .map_err(crate::RtpTransceiverOperationError::Binding)?;
        let mut safe = false;
        let native =
            ffi::rtp_transceiver_set_direction_value(self.native(), direction.raw(), &mut safe);
        if !safe {
            return Err(crate::RtpTransceiverOperationError::UnsafeDirection(
                direction.raw(),
            ));
        }
        Self::capability_install_result(native)
    }
    /// The public synchronous SDK stop operation, preserving native RTCError
    /// and idempotent already-stopping behavior. This is not StopInternal.
    pub fn stop_standard(&self) -> Result<(), crate::RtpTransceiverOperationError> {
        self.check_capability_peer_open()
            .map_err(crate::RtpTransceiverOperationError::Binding)?;
        Self::capability_install_result(ffi::rtp_transceiver_stop_standard(self.native()))
    }

    /// Legacy four-state convenience direction.
    pub fn direction(&self) -> RtpTransceiverDirection {
        direction(ffi::rtp_transceiver_direction(self.native()))
    }

    /// Legacy narrowed direction metadata. Use current_direction_value for the
    /// complete native enum and explicit optional presence.
    pub fn current_direction(&self) -> Option<RtpTransceiverDirection> {
        u8::try_from(ffi::rtp_transceiver_current_direction(self.native()))
            .ok()
            .map(direction)
    }

    pub fn stopped(&self) -> bool {
        ffi::rtp_transceiver_stopped(self.native())
    }

    /// Complete owned SDK codec preferences, not the legacy format DTOs.
    /// Empty means no override; this does not synthesize factory defaults.
    pub fn rtp_codec_preferences(&self) -> Result<Vec<crate::RtpCodecCapability>, PeerError> {
        self.check_capability_peer_open()?;
        Ok(
            crate::RtpCapabilities::from_native(ffi::rtp_transceiver_codec_preferences_value(
                self.native(),
            ))
            .codecs(),
        )
    }

    /// Complete configured SDK header-extension values for the next negotiation.
    /// Returned entries and their children are independent owned metadata.
    pub fn header_extension_capabilities_to_negotiate(
        &self,
    ) -> Result<Vec<crate::RtpHeaderExtensionCapabilityValue>, PeerError> {
        self.header_extension_capabilities(false)
    }

    /// Complete negotiated SDK snapshot. Unmatched entries are marked Stopped;
    /// pre-negotiation snapshots need not be empty. IDs and encryption are the
    /// actual SDK metadata, not sentinel-reconstructed or filtered DTO fields.
    pub fn negotiated_header_extension_capabilities(
        &self,
    ) -> Result<Vec<crate::RtpHeaderExtensionCapabilityValue>, PeerError> {
        self.header_extension_capabilities(true)
    }

    fn header_extension_capabilities(
        &self,
        negotiated: bool,
    ) -> Result<Vec<crate::RtpHeaderExtensionCapabilityValue>, PeerError> {
        self.check_capability_peer_open()?;
        Ok(
            crate::RtpCapabilities::from_native(ffi::rtp_transceiver_header_capabilities_value(
                self.native(),
                negotiated,
            ))
            .header_extensions(),
        )
    }

    /// Pass complete codec values to the SDK, including all inherited metadata.
    /// Empty clears the override. SDK validation and deduplication are retained;
    /// this does not substitute sender-factory capabilities by name/parameters.
    /// SDK failures retain their complete owning RTCError.
    pub fn set_rtp_codec_preferences(
        &self,
        codecs: &[crate::RtpCodecCapability],
    ) -> Result<(), crate::RtpCapabilityInstallError> {
        self.check_capability_peer_open()
            .map_err(crate::RtpCapabilityInstallError::Binding)?;
        let mut values = crate::RtpCapabilities::default();
        values.set_codecs(codecs);
        Self::capability_install_result(ffi::rtp_transceiver_set_codec_preferences_value(
            self.native(),
            values.native(),
        ))
    }

    /// Pass the complete ordered configured snapshot to the SDK. The native
    /// setter validates URI/order and mandatory extensions, then changes only
    /// direction. Supplied IDs/encryption are metadata, not installation requests.
    /// Unknown directions are retained where SDK semantics allow; this path
    /// neither formats them nor converts them through the legacy closed DTO.
    pub fn set_header_extension_capabilities_to_negotiate(
        &self,
        extensions: &[crate::RtpHeaderExtensionCapabilityValue],
    ) -> Result<(), crate::RtpCapabilityInstallError> {
        self.check_capability_peer_open()
            .map_err(crate::RtpCapabilityInstallError::Binding)?;
        let mut values = crate::RtpCapabilities::default();
        values.set_header_extensions(extensions);
        Self::capability_install_result(ffi::rtp_transceiver_set_header_capabilities_value(
            self.native(),
            values.native(),
        ))
    }

    fn capability_install_result(
        native: cxx::UniquePtr<ffi::NativeRtcError>,
    ) -> Result<(), crate::RtpCapabilityInstallError> {
        let error = crate::RtcError::from_native(native);
        if error.is_ok() {
            Ok(())
        } else {
            Err(crate::RtpCapabilityInstallError::Native(error))
        }
    }

    fn check_capability_peer_open(&self) -> Result<(), PeerError> {
        if self.peer.closed.get() {
            Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer is closed".into(),
            })
        } else {
            Ok(())
        }
    }

    /// Legacy configured extension DTO for the next SDP negotiation.
    pub fn header_extensions_to_negotiate(
        &self,
    ) -> Result<Vec<RtpHeaderExtensionCapability>, PeerError> {
        self.header_extensions(false)
    }

    /// Native negotiated extension snapshot. Entries marked `Stopped` are not
    /// negotiated. Before negotiation the native snapshot may contain stopped
    /// entries rather than an empty vector.
    pub fn negotiated_header_extensions(
        &self,
    ) -> Result<Vec<RtpHeaderExtensionCapability>, PeerError> {
        self.header_extensions(true)
    }

    fn header_extensions(
        &self,
        negotiated: bool,
    ) -> Result<Vec<RtpHeaderExtensionCapability>, PeerError> {
        if self.peer.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer is closed".into(),
            });
        }
        ffi::rtp_transceiver_header_extensions(self.native(), negotiated)
            .into_iter()
            .map(|value| {
                let direction = match value.direction {
                    0 => RtpHeaderExtensionDirection::SendReceive,
                    1 => RtpHeaderExtensionDirection::SendOnly,
                    2 => RtpHeaderExtensionDirection::ReceiveOnly,
                    3 => RtpHeaderExtensionDirection::Inactive,
                    4 => RtpHeaderExtensionDirection::Stopped,
                    _ => {
                        return Err(PeerError {
                            kind: PeerErrorKind::Internal,
                            message: "unknown native extension direction".into(),
                        });
                    }
                };
                Ok(RtpHeaderExtensionCapability {
                    uri: value.uri,
                    preferred_id: (value.preferred_id >= 0).then_some(value.preferred_id),
                    preferred_encrypt: value.preferred_encrypt,
                    direction,
                })
            })
            .collect()
    }

    /// Apply directions to the full ordered snapshot returned by
    /// [`Self::header_extensions_to_negotiate`]. Native validation checks the
    /// list and mandatory extensions. IDs and encryption remain native-owned.
    /// This changes the next negotiation, not already negotiated transport.
    pub fn set_header_extensions_to_negotiate(
        &self,
        extensions: &[RtpHeaderExtensionCapability],
    ) -> Result<(), PeerError> {
        if self.peer.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer is closed".into(),
            });
        }
        let extensions: Vec<_> = extensions
            .iter()
            .map(|value| ffi::FfiRtpHeaderExtension {
                uri: value.uri.clone(),
                preferred_id: value.preferred_id.unwrap_or(-1),
                preferred_encrypt: value.preferred_encrypt,
                direction: value.direction as u8,
            })
            .collect();
        let mut kind = 0;
        let mut error = String::new();
        if ffi::rtp_transceiver_set_header_extensions(
            self.native(),
            &extensions,
            &mut kind,
            &mut error,
        ) {
            Ok(())
        } else {
            Err(PeerError {
                kind: error_kind(kind),
                message: error,
            })
        }
    }

    /// The negotiated media ID, absent before negotiation or after rollback.
    pub fn mid(&self) -> Option<String> {
        let mut mid = String::new();
        ffi::rtp_transceiver_mid(self.native(), &mut mid).then_some(mid)
    }

    /// Order supported video codecs for this transceiver's next negotiation.
    /// An empty list restores upstream defaults. Pass capabilities returned by
    /// the peer, rather than inventing payload profiles or fmtp parameters.
    pub fn set_codec_preferences(&self, codecs: &[VideoCodecCapability]) -> Result<(), PeerError> {
        if self.peer.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer is closed".into(),
            });
        }
        let formats: Vec<_> = codecs
            .iter()
            .map(VideoCodecCapability::ffi_format)
            .collect();
        let mut error_type = 0;
        let mut message = String::new();
        ffi::rtp_transceiver_set_video_codec_preferences(
            self.native(),
            self.peer.factory_native(),
            &formats,
            &mut error_type,
            &mut message,
        )
        .then_some(())
        .ok_or_else(|| PeerError {
            kind: error_kind(error_type),
            message,
        })
    }

    /// Order supported audio codecs for this transceiver's next negotiation.
    /// Stereo Opus also requires the receiver to answer with `stereo=1` on
    /// the matching audio m-line; preferences alone do not request stereo.
    pub fn set_audio_codec_preferences(
        &self,
        codecs: &[AudioCodecCapability],
    ) -> Result<(), PeerError> {
        if self.peer.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer is closed".into(),
            });
        }
        let formats: Vec<_> = codecs
            .iter()
            .map(AudioCodecCapability::ffi_format)
            .collect();
        let mut error_type = 0;
        let mut message = String::new();
        ffi::rtp_transceiver_set_audio_codec_preferences(
            self.native(),
            self.peer.factory_native(),
            &formats,
            &mut error_type,
            &mut message,
        )
        .then_some(())
        .ok_or_else(|| PeerError {
            kind: error_kind(error_type),
            message,
        })
    }

    /// Start standard transceiver stopping; negotiate again to complete it.
    pub fn stop(&self) -> Result<(), PeerError> {
        if self.peer.closed.get() {
            return Err(PeerError {
                kind: PeerErrorKind::Closed,
                message: "peer is closed".into(),
            });
        }
        let mut error_type = 0;
        let mut message = String::new();
        ffi::rtp_transceiver_stop(self.native(), &mut error_type, &mut message)
            .then_some(())
            .ok_or_else(|| PeerError {
                kind: error_kind(error_type),
                message,
            })
    }

    pub fn set_direction(&self, direction: RtpTransceiverDirection) -> Result<(), PeerError> {
        let mut error_type = 0;
        let mut message = String::new();
        ffi::rtp_transceiver_set_direction(
            self.native(),
            direction as u8,
            &mut error_type,
            &mut message,
        )
        .then_some(())
        .ok_or_else(|| PeerError {
            kind: error_kind(error_type),
            message,
        })
    }

    fn native(&self) -> &ffi::NativeRtpTransceiver {
        self.native.as_ref().expect("validated RTP transceiver")
    }
}

fn direction(value: u8) -> RtpTransceiverDirection {
    match value {
        0 => RtpTransceiverDirection::SendReceive,
        1 => RtpTransceiverDirection::SendOnly,
        2 => RtpTransceiverDirection::ReceiveOnly,
        3 => RtpTransceiverDirection::Inactive,
        _ => unreachable!("native adapter returned an invalid RTP direction"),
    }
}

fn native_error(message: &str) -> PeerError {
    PeerError {
        kind: crate::PeerErrorKind::NativeConstruction,
        message: message.into(),
    }
}

#[cfg(test)]
mod allocation_tests {
    use crate::ffi;

    #[test]
    fn raw_source_preflight_matches_the_pinned_i420_allocator_int_bound() {
        let limit = i32::MAX as u32;
        // Exercise the actual helper called before I420Buffer::Create, without
        // allocating multi-gigabyte fixtures or entering a native CHECK.
        for (width, height) in [
            (0, 1),
            (1, 0),
            (1, 1),
            (2, 2),
            (3, 3),
            (17, 9),
            (1, limit / 2),
            (1, limit / 2 + 1),
            (limit / 2, 1),
            (limit / 2 + 1, 1),
            (2, limit / 3),
            (2, limit / 3 + 1),
            (limit / 3, 2),
            (limit / 3 + 1, 2),
            (65536, 65536),
            (limit, 1),
            (1, limit),
            (limit, limit),
            (limit + 1, 1),
            (u32::MAX, u32::MAX),
        ] {
            let (w, h) = (u128::from(width), u128::from(height));
            let size = w * h + 2 * w.div_ceil(2) * h.div_ceil(2);
            let expected = if width != 0
                && height != 0
                && width <= limit
                && height <= limit
                && size <= u128::from(limit)
            {
                size as usize
            } else {
                0
            };
            assert_eq!(
                ffi::video_source_i420_allocation_size(width, height),
                expected,
                "{width}x{height}"
            );
        }
        assert_eq!(
            ffi::video_source_i420_allocation_size(1, limit / 2),
            limit as usize
        );
        assert_eq!(
            ffi::video_source_i420_allocation_size(2, limit / 3),
            limit as usize - 1
        );
    }
}
