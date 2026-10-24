//! Receiver-scoped native streams. Catalog assignments are deliberately absent.
use pulsebeam_webrtc_sys as rtc;

use crate::native_media::{
    DecodeTargetIndication, FrameMetadata, I420Frame, OutputFrame, ReceiveMode, RemoteFrame,
    RemoteTrack,
};

fn decoded_frame(frame: rtc::VideoFrame) -> (OutputFrame, FrameMetadata) {
    (
        OutputFrame::I420 {
            frame: I420Frame {
                width: frame.width,
                height: frame.height,
                timestamp_us: frame.timestamp_us,
                data: frame.buffer.into_bytes(),
                rotation_degrees: frame.rotation as u16,
            },
        },
        FrameMetadata {
            rtp_timestamp: Some(frame.rtp_timestamp),
            ..FrameMetadata::default()
        },
    )
}

pub(crate) struct NativeReceiver {
    pub id: rtc::SessionReceiverId,
    pub track: RemoteTrack,
    pub kind: rtc::MediaType,
    pub mode: Option<ReceiveMode>,
    prefer_decoded: bool,
}

impl NativeReceiver {
    pub fn new<R: rtc::SessionRuntime>(
        session: &mut rtc::Session<R>,
        id: rtc::SessionReceiverId,
        generation: u64,
        mid: String,
        media_index: u32,
    ) -> Result<Self, rtc::PeerError> {
        let track = session
            .receiver_track_reference(id)?
            .ok_or_else(|| rtc::PeerError {
                kind: rtc::PeerErrorKind::InvalidState,
                message: "native receiver has no track".into(),
            })?;
        let track_id = session.media_track_id(track);
        let released = session.release_media_track_reference(track);
        let track_id = track_id?;
        released?;
        let track_id = String::from_utf8(track_id).map_err(|error| rtc::PeerError {
            kind: rtc::PeerErrorKind::InvalidParameter,
            message: format!("native track ID is not UTF-8: {error}"),
        })?;
        Ok(Self {
            id,
            track: RemoteTrack {
                generation,
                receiver: id.get(),
                track_id,
                mid,
                media_index,
            },
            kind: session.receiver_media_type(id)?,
            mode: None,
            prefer_decoded: false,
        })
    }

    pub fn attach<R: rtc::SessionRuntime>(
        &mut self,
        session: &mut rtc::Session<R>,
        mode: ReceiveMode,
        observer: Option<&crate::DecodedVideoObserver>,
    ) -> Result<(), rtc::PeerError> {
        if self.mode == Some(mode) {
            return Ok(());
        }
        if self.mode.is_some() {
            return Err(rtc::PeerError {
                kind: rtc::PeerErrorKind::InvalidState,
                message: "detach the current receiver mode before attaching another".into(),
            });
        }
        let callback = observer.map(|observer| {
            let observer = observer.clone();
            let track = self.track.clone();
            move |native: rtc::DecodedVideoFrameObservation| {
                let frame = native
                    .frame
                    .map(|frame| {
                        let (frame, metadata) = decoded_frame(frame);
                        RemoteFrame {
                            track: track.clone(),
                            metadata,
                            frame,
                        }
                    })
                    .map_err(|error| format!("{error:?}"));
                observer.observe(crate::DecodedVideoObservation {
                    track: track.clone(),
                    observed_at: native.observed_at,
                    native_observed_at_us: native.native_observed_at_us,
                    frame,
                });
            }
        });
        match (self.kind, mode, callback) {
            (rtc::MediaType::Audio, ReceiveMode::Decoded, _) => session.attach_pcm(self.id)?,
            (rtc::MediaType::Audio, ReceiveMode::Encoded, _) => {
                session.attach_encoded_audio(self.id)?;
            }
            (rtc::MediaType::Video, ReceiveMode::Decoded, Some(callback)) => {
                session.attach_raw_video_with_observer(self.id, callback)?;
            }
            (rtc::MediaType::Video, ReceiveMode::Decoded, None) => {
                session.attach_raw_video(self.id)?;
            }
            (rtc::MediaType::Video, ReceiveMode::Encoded, _) => {
                session.attach_encoded_video(self.id)?;
            }
            (rtc::MediaType::Video, ReceiveMode::EncodedAndDecoded, Some(callback)) => {
                session.attach_raw_and_encoded_video_with_observer(self.id, callback)?;
            }
            (rtc::MediaType::Video, ReceiveMode::EncodedAndDecoded, None) => {
                session.attach_raw_and_encoded_video(self.id)?;
            }
            _ => {
                return Err(rtc::PeerError {
                    kind: rtc::PeerErrorKind::UnsupportedParameter,
                    message: "unsupported receiver kind".into(),
                });
            }
        }
        self.mode = Some(mode);
        self.prefer_decoded = false;
        Ok(())
    }

    pub fn detach<R: rtc::SessionRuntime>(
        &mut self,
        session: &mut rtc::Session<R>,
    ) -> Result<(), rtc::PeerError> {
        session.close_media_sink(self.id)?;
        self.mode = None;
        Ok(())
    }

    /// PCM is an explicit native 10 ms playout pull, not a queued-frame pop.
    /// Its caller must schedule one pull per native playout interval. Repeated
    /// calls in a drain loop would incorrectly manufacture playout progress.
    pub fn next<R: rtc::SessionRuntime>(
        &mut self,
        session: &mut rtc::Session<R>,
        playout_due: bool,
    ) -> Result<Option<RemoteFrame>, rtc::PeerError> {
        if self.kind == rtc::MediaType::Video
            && matches!(
                self.mode,
                Some(ReceiveMode::Decoded | ReceiveMode::EncodedAndDecoded)
            )
            && session.raw_video_observer_failed(self.id)?
        {
            return Err(rtc::PeerError {
                kind: rtc::PeerErrorKind::InvalidState,
                message: "native decoded callback observer panicked and retired".into(),
            });
        }
        if self.kind == rtc::MediaType::Video && self.mode == Some(ReceiveMode::EncodedAndDecoded) {
            // Fairly drain two independent native observer queues. This never
            // sorts/reorders media or manufactures either kind of output.
            let modes = if self.prefer_decoded {
                [ReceiveMode::Decoded, ReceiveMode::Encoded]
            } else {
                [ReceiveMode::Encoded, ReceiveMode::Decoded]
            };
            self.prefer_decoded = !self.prefer_decoded;
            if let Some(frame) = self.next_in_mode(session, Some(modes[0]), playout_due)? {
                return Ok(Some(frame));
            }
            return self.next_in_mode(session, Some(modes[1]), playout_due);
        }
        self.next_in_mode(session, self.mode, playout_due)
    }

    fn next_in_mode<R: rtc::SessionRuntime>(
        &self,
        session: &mut rtc::Session<R>,
        mode: Option<ReceiveMode>,
        playout_due: bool,
    ) -> Result<Option<RemoteFrame>, rtc::PeerError> {
        let value = match (self.kind, mode) {
            (rtc::MediaType::Audio, Some(ReceiveMode::Decoded)) if playout_due => {
                session.try_pcm_frame(self.id)?.map(|frame| {
                    (
                        OutputFrame::Pcm {
                            sample_rate_hz: frame.sample_rate_hz,
                            channels: frame.channels,
                            samples: frame.samples,
                        },
                        FrameMetadata {
                            capture_time_ms: frame.capture_time_ms,
                            capture_time_us: frame.capture_time_us,
                            samples_per_channel: Some(frame.samples_per_channel),
                            ..FrameMetadata::default()
                        },
                    )
                })
            }
            (rtc::MediaType::Audio, Some(ReceiveMode::Encoded)) => {
                session.try_audio_frame(self.id)?.map(|frame| {
                    (
                        OutputFrame::Opus {
                            timestamp: frame.rtp_timestamp,
                            payload: frame.data,
                        },
                        FrameMetadata {
                            rtp_timestamp: Some(frame.rtp_timestamp),
                            sequence_number: frame.sequence_number,
                            ssrc: Some(frame.ssrc),
                            payload_type: Some(frame.payload_type),
                            audio_level_dbov: frame.audio_level_dbov,
                            voice_activity: frame.voice_activity,
                            samples_per_channel: Some(frame.samples_per_channel),
                            capture_time_us: frame.capture_time_us,
                            receive_time_us: frame.receive_time_us,
                            ..FrameMetadata::default()
                        },
                    )
                })
            }
            (rtc::MediaType::Video, Some(ReceiveMode::Decoded)) => {
                session.try_raw_video_frame(self.id)?.map(decoded_frame)
            }
            (rtc::MediaType::Video, Some(ReceiveMode::Encoded)) => {
                session.try_video_frame(self.id)?.map(|frame| {
                    (
                        OutputFrame::H264 {
                            timestamp: frame.rtp_timestamp,
                            keyframe: frame.key_frame,
                            payload: frame.data,
                        },
                        FrameMetadata {
                            rtp_timestamp: Some(frame.rtp_timestamp),
                            ssrc: Some(frame.ssrc),
                            payload_type: Some(frame.payload_type),
                            codec_mime_type: Some(frame.mime_type),
                            rid: frame.rid,
                            capture_time_us: frame.capture_time_us,
                            receive_time_us: frame.receive_time_us,
                            frame_id: frame.frame_id,
                            spatial_index: frame.spatial_index,
                            temporal_index: frame.temporal_index,
                            dependencies: frame.dependencies,
                            decode_target_indications: frame
                                .decode_target_indications
                                .into_iter()
                                .map(|value| match value {
                                    rtc::DecodeTargetIndication::NotPresent => {
                                        DecodeTargetIndication::NotPresent
                                    }
                                    rtc::DecodeTargetIndication::Discardable => {
                                        DecodeTargetIndication::Discardable
                                    }
                                    rtc::DecodeTargetIndication::Switch => {
                                        DecodeTargetIndication::Switch
                                    }
                                    rtc::DecodeTargetIndication::Required => {
                                        DecodeTargetIndication::Required
                                    }
                                })
                                .collect(),
                            ..FrameMetadata::default()
                        },
                    )
                })
            }
            _ => None,
        };
        Ok(value.map(|(frame, metadata)| RemoteFrame {
            track: self.track.clone(),
            metadata,
            frame,
        }))
    }
}
