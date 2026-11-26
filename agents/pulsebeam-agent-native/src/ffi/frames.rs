use super::*;
use crate::native_actor::frames::{FrameReader, FrameState};

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct VideoFrame {
    pub track: crate::NativeReceiverInfo,
    pub metadata: crate::FrameMetadata,
    pub frame: crate::I420Frame,
}

#[derive(Clone, Debug, PartialEq, uniffi::Record)]
pub struct AudioFrame {
    pub track: crate::NativeReceiverInfo,
    pub metadata: crate::FrameMetadata,
    pub sample_rate_hz: u32,
    pub channels: u8,
    pub samples: Vec<i16>,
}

#[derive(uniffi::Object)]
pub struct VideoFrames {
    runtime: FrameReader,
}

impl VideoFrames {
    pub(super) fn admitted(
        room: crate::RoomRef,
        admission: agent_core::AdmittedMedia,
    ) -> Result<Self, NativeError> {
        Ok(Self {
            runtime: FrameReader::admitted(room, admission).map_err(operation_error)?,
        })
    }

    pub(super) fn new(
        subscription: RuntimeSubscription,
        observer: Arc<agent_core::observer::VideoSubscriptionStateObserver>,
    ) -> Result<Self, NativeError> {
        Ok(Self {
            runtime: FrameReader::new(subscription, FrameState::Video(observer))
                .map_err(operation_error)?,
        })
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl VideoFrames {
    pub async fn next(&self) -> Result<Option<VideoFrame>, NativeError> {
        let Some(frame) = self.runtime.next().await.map_err(operation_error)? else {
            return Ok(None);
        };
        let crate::OutputFrame::I420 { frame: pixels } = frame.frame else {
            return Err(NativeError::Operation {
                info: core_ffi::ErrorInfo::new(
                    core_ffi::ErrorCode::Unsupported,
                    "Decoded video frames are unavailable",
                ),
            });
        };
        Ok(Some(VideoFrame {
            track: frame.track,
            metadata: frame.metadata,
            frame: pixels,
        }))
    }

    pub fn close(&self) {
        self.runtime.close();
    }
}

#[derive(uniffi::Object)]
pub struct AudioFrames {
    runtime: FrameReader,
}

impl AudioFrames {
    pub(super) fn admitted(
        room: crate::RoomRef,
        admission: agent_core::AdmittedMedia,
    ) -> Result<Self, NativeError> {
        Ok(Self {
            runtime: FrameReader::admitted(room, admission).map_err(operation_error)?,
        })
    }

    pub(super) fn new(
        subscription: RuntimeSubscription,
        observer: Arc<agent_core::observer::AudioSubscriptionStateObserver>,
    ) -> Result<Self, NativeError> {
        Ok(Self {
            runtime: FrameReader::new(subscription, FrameState::Audio(observer))
                .map_err(operation_error)?,
        })
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl AudioFrames {
    pub async fn next(&self) -> Result<Option<AudioFrame>, NativeError> {
        let Some(frame) = self.runtime.next().await.map_err(operation_error)? else {
            return Ok(None);
        };
        let crate::OutputFrame::Pcm {
            sample_rate_hz,
            channels,
            samples,
        } = frame.frame
        else {
            return Err(NativeError::Operation {
                info: core_ffi::ErrorInfo::new(
                    core_ffi::ErrorCode::Unsupported,
                    "Decoded audio frames are unavailable",
                ),
            });
        };
        Ok(Some(AudioFrame {
            track: frame.track,
            metadata: frame.metadata,
            sample_rate_hz,
            channels,
            samples,
        }))
    }

    pub fn close(&self) {
        self.runtime.close();
    }
}
