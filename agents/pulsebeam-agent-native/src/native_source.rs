//! Session-owned sources shared by the production and managed RTC actors.
use pulsebeam_webrtc_sys as rtc;

use crate::native_media::{
    AudioInput, Encoding, H264Frame, InputFrame, VideoInput, VideoSourceConfig,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceKind {
    Audio(AudioInput),
    Video(VideoInput),
}

pub(crate) struct NativeSource {
    pub id: rtc::SessionSourceId,
    pub sender: rtc::SessionSenderId,
    pub transceiver: rtc::SessionTransceiverId,
    pub kind: SourceKind,
    pub encodings: Vec<Encoding>,
    pub active: bool,
}

fn invalid(message: impl Into<String>) -> rtc::PeerError {
    rtc::PeerError {
        kind: rtc::PeerErrorKind::InvalidParameter,
        message: message.into(),
    }
}

fn native(error: impl std::fmt::Display) -> rtc::PeerError {
    rtc::PeerError {
        kind: rtc::PeerErrorKind::Internal,
        message: error.to_string(),
    }
}

pub(crate) fn encoding_values(
    config: &VideoSourceConfig,
) -> Result<Vec<rtc::RtpEncodingParameters>, rtc::PeerError> {
    if !(1..=3).contains(&config.encodings.len()) {
        return Err(invalid("video requires one to three encodings"));
    }
    if !config.dependency_descriptor
        && config
            .encodings
            .iter()
            .any(|value| value.temporal_layers > 1)
    {
        return Err(invalid("layered H264 requires dependency descriptors"));
    }
    config
        .encodings
        .iter()
        .map(|value| {
            if !matches!(value.temporal_layers, 1 | 3) {
                return Err(invalid("H264 supports L1T1 or L1T3"));
            }
            if config.encodings.len() > 1 && value.temporal_layers != 1 {
                return Err(invalid("H264 simulcast requires L1T1 per encoding"));
            }
            let mut encoding = rtc::RtpEncodingParameters::default();
            if let Some(rid) = &value.rid {
                encoding.set_rid(rid.as_bytes());
            }
            encoding.set_scale_resolution_down_by(Some(value.scale_down_by));
            encoding.set_max_bitrate_bps(
                value
                    .max_bitrate_bps
                    .map(i32::try_from)
                    .transpose()
                    .map_err(|_| invalid("bitrate exceeds the native i32 range"))?,
            );
            encoding.set_max_framerate(value.max_framerate);
            encoding.set_num_temporal_layers(Some(i32::from(value.temporal_layers)));
            // The direct L1T1 source contract declares one layer without a
            // scalability-mode override. L1T3 requires the explicit mode.
            encoding
                .set_scalability_mode((value.temporal_layers == 3).then_some(b"L1T3".as_slice()));
            Ok(encoding)
        })
        .collect()
}

impl NativeSource {
    pub fn create<R: rtc::SessionRuntime>(
        session: &mut rtc::Session<R>,
        peer: rtc::SessionPeerId,
        track_id: &str,
        kind: SourceKind,
        video: &VideoSourceConfig,
    ) -> Result<Self, rtc::PeerError> {
        let encodings = match kind {
            SourceKind::Audio(_) => Vec::new(),
            SourceKind::Video(_) => encoding_values(video)?,
        };
        let id = Self::allocate(session, kind, &video.encodings)?;
        let mut init = rtc::RtpTransceiverInit::default();
        init.set_direction(rtc::RtpTransceiverDirectionValue::SendOnly);
        init.set_send_encodings(&encodings);
        let result = match kind {
            SourceKind::Audio(_) => session.publish_audio_with_init(peer, id, track_id, &init),
            SourceKind::Video(_) => session.publish_video_with_init(peer, id, track_id, &init),
        };
        let transceiver = match result {
            Ok(value) => value,
            Err(error) => {
                session.close_source(id)?;
                return Err(native(error));
            }
        };
        let sender = session.sender(transceiver)?;
        let media_type = match kind {
            SourceKind::Audio(_) => rtc::MediaType::Audio,
            SourceKind::Video(_) => rtc::MediaType::Video,
        };
        let capabilities = session.get_rtp_sender_capabilities(media_type)?;
        let codecs: Vec<_> = capabilities
            .codecs()
            .into_iter()
            .filter(|codec| {
                let name = codec.base_codec().name_bytes();
                match kind {
                    SourceKind::Audio(_) => name.eq_ignore_ascii_case(b"opus"),
                    SourceKind::Video(_) => {
                        name.eq_ignore_ascii_case(b"H264") || name.eq_ignore_ascii_case(b"rtx")
                    }
                }
            })
            .collect();
        session
            .set_rtp_codec_preferences(transceiver, &codecs)
            .map_err(native)?;
        Ok(Self {
            id,
            sender,
            transceiver,
            kind,
            encodings: video.encodings.clone(),
            active: true,
        })
    }

    fn allocate<R: rtc::SessionRuntime>(
        session: &mut rtc::Session<R>,
        kind: SourceKind,
        encodings: &[Encoding],
    ) -> Result<rtc::SessionSourceId, rtc::PeerError> {
        match kind {
            SourceKind::Audio(AudioInput::Pcm) => session.create_pcm_source(),
            SourceKind::Audio(AudioInput::EncodedOpus { channels }) => {
                session.create_opus_source(channels)
            }
            SourceKind::Video(VideoInput::I420) => session.create_raw_video_source(),
            SourceKind::Video(VideoInput::EncodedH264) => {
                let profile = match encodings {
                    [value] if value.temporal_layers == 1 => rtc::H264SourceProfile::L1T1,
                    [_] => rtc::H264SourceProfile::L1T3,
                    [_, _, _] => rtc::H264SourceProfile::SimulcastL1T1,
                    _ => {
                        return Err(invalid(
                            "native encoded H264 currently admits one or three encodings",
                        ));
                    }
                };
                session.create_h264_source_with_profile(profile)
            }
        }
    }

    /// Replace only the producer. Native sender/transceiver identity is stable.
    pub fn replace<R: rtc::SessionRuntime>(
        &mut self,
        session: &mut rtc::Session<R>,
        track_id: &str,
    ) -> Result<(), rtc::PeerError> {
        let id = Self::allocate(session, self.kind, &self.encodings)?;
        let replaced = match self.kind {
            SourceKind::Audio(_) => session.replace_audio_source(self.sender, Some(id), track_id),
            SourceKind::Video(_) => session.replace_video_source(self.sender, Some(id), track_id),
        };
        if let Err(error) = replaced {
            session.close_source(id)?;
            return Err(error);
        }
        let old = std::mem::replace(&mut self.id, id);
        self.active = true;
        session.close_source(old)
    }

    pub fn set_enabled<R: rtc::SessionRuntime>(
        &self,
        session: &mut rtc::Session<R>,
        enabled: bool,
    ) -> Result<bool, rtc::PeerError> {
        let track = session
            .sender_track_reference(self.sender)?
            .ok_or_else(|| rtc::PeerError {
                kind: rtc::PeerErrorKind::Closed,
                message: "source is detached".into(),
            })?;
        let result = (|| {
            if !session.set_media_track_enabled(track, enabled)? {
                return Err(rtc::PeerError {
                    kind: rtc::PeerErrorKind::Operation,
                    message: "native track refused enable/mute".into(),
                });
            }
            session.media_track_enabled(track)
        })();
        let released = session.release_media_track_reference(track);
        let enabled = result?;
        released?;
        Ok(enabled)
    }

    pub fn encoding_state<R: rtc::SessionRuntime>(
        &self,
        session: &mut rtc::Session<R>,
    ) -> Result<Vec<crate::EncodingState>, rtc::PeerError> {
        Ok(session
            .get_sender_parameters(self.sender)?
            .encodings()
            .into_iter()
            .map(|encoding| crate::EncodingState {
                rid: encoding.rid_bytes(),
                active: encoding.active(),
                max_bitrate_bps: encoding.max_bitrate_bps(),
                max_framerate: encoding.max_framerate(),
                scale_down_by: encoding.scale_resolution_down_by(),
                temporal_layers: encoding.num_temporal_layers(),
                scalability_mode: encoding.scalability_mode_bytes(),
            })
            .collect())
    }

    pub fn set_limits<R: rtc::SessionRuntime>(
        &self,
        session: &mut rtc::Session<R>,
        limits: &[crate::EncodingLimits],
    ) -> Result<Vec<crate::EncodingState>, rtc::PeerError> {
        let mut parameters = session.get_sender_parameters(self.sender)?;
        let mut encodings = parameters.encodings();
        if limits.len() != encodings.len() {
            return Err(invalid("limit count differs from actual native encodings"));
        }
        for (encoding, limits) in encodings.iter_mut().zip(limits) {
            encoding.set_max_bitrate_bps(
                limits
                    .max_bitrate_bps
                    .map(i32::try_from)
                    .transpose()
                    .map_err(|_| invalid("bitrate exceeds native i32 range"))?,
            );
            encoding.set_max_framerate(limits.max_framerate);
        }
        parameters.set_encodings(&encodings);
        let result = session.set_sender_parameters_value(self.sender, &parameters)?;
        if !result.is_ok() {
            return Err(rtc::PeerError {
                kind: rtc::PeerErrorKind::Operation,
                message: format!("native sender parameter error: {result}"),
            });
        }
        self.encoding_state(session)
    }

    pub fn feedback<R: rtc::SessionRuntime>(
        &self,
        session: &mut rtc::Session<R>,
    ) -> Result<crate::ProducerFeedback, rtc::PeerError> {
        if self.kind != SourceKind::Video(VideoInput::EncodedH264) {
            return Err(rtc::PeerError {
                kind: rtc::PeerErrorKind::UnsupportedOperation,
                message: "producer encoder feedback is specific to encoded H264 input".into(),
            });
        }
        let (keyframe_requested, rates) = session.video_feedback(self.id)?;
        let encoding_keyframes_requested = session
            .video_encoding_keyframe_requests(self.id)?
            .into_iter()
            .take(self.encodings.len())
            .collect();
        let encoder_error = session
            .take_video_encoder_error(self.id)?
            .map(crate::VideoEncoderError::from);
        Ok(crate::ProducerFeedback {
            keyframe_requested,
            encoding_keyframes_requested,
            encoder_error,
            rates: rates.map(|rates| crate::VideoRateControl {
                bitrate_bps: rates.bitrate_bps,
                target_bitrate_bps: rates.target_bitrate_bps,
                framerate_fps: rates.framerate_fps,
                bandwidth_bps: rates.bandwidth_bps,
                bandwidth_limited: rates.bandwidth_limited,
                target_bandwidth_limited: rates.target_bandwidth_limited,
                layer_bitrates_bps: rates
                    .layer_bitrates_bps
                    .into_iter()
                    .map(Vec::from)
                    .collect(),
                target_layer_bitrates_bps: rates
                    .target_layer_bitrates_bps
                    .into_iter()
                    .map(Vec::from)
                    .collect(),
            }),
        })
    }

    pub fn push<R: rtc::SessionRuntime>(
        &self,
        session: &mut rtc::Session<R>,
        frame: InputFrame,
    ) -> Result<(), rtc::PeerError> {
        match (self.kind, frame) {
            (SourceKind::Audio(AudioInput::Pcm), InputFrame::Pcm { frame }) => {
                let channels = u8::try_from(frame.channels)
                    .map_err(|_| invalid("invalid PCM channel count"))?;
                let frame = rtc::AudioPcmFrame::i16_interleaved(
                    frame.sample_rate_hz,
                    channels,
                    frame.timestamp_us,
                    frame.samples,
                )
                .map_err(native)?;
                session.push_pcm(self.id, &frame)
            }
            (SourceKind::Audio(AudioInput::EncodedOpus { .. }), InputFrame::Opus { frame }) => {
                let level = frame
                    .audio_level
                    .map(|level| rtc::OpusAudioLevel::new(level, frame.voice_activity))
                    .transpose()
                    .map_err(native)?;
                let frame = rtc::OpusInputFrame {
                    data: frame.payload,
                    rtp_timestamp: frame.timestamp,
                    samples_per_channel: frame.samples_per_channel,
                };
                if let Some(level) = level {
                    session.push_opus_with_audio_level(self.id, &frame, level)
                } else {
                    session.push_opus(self.id, &frame)
                }
            }
            (SourceKind::Video(VideoInput::I420), InputFrame::I420 { frame }) => {
                let rotation = match frame.rotation_degrees {
                    0 => rtc::VideoRotation::None,
                    90 => rtc::VideoRotation::Clockwise90,
                    180 => rtc::VideoRotation::Clockwise180,
                    270 => rtc::VideoRotation::Clockwise270,
                    _ => return Err(invalid("rotation must be 0, 90, 180 or 270 degrees")),
                };
                let frame = rtc::VideoFrame::i420(
                    frame.width,
                    frame.height,
                    frame.data,
                    frame.timestamp_us,
                    0,
                )
                .map_err(native)?
                .with_rotation(rotation);
                session.push_raw_video(self.id, &frame)
            }
            (SourceKind::Video(VideoInput::EncodedH264), InputFrame::H264 { frame })
                if self.encodings.len() == 1 =>
            {
                let temporal_layers = self
                    .encodings
                    .first()
                    .ok_or_else(|| invalid("source has no encoding"))?
                    .temporal_layers;
                session.push_video(self.id, video_unit(frame, None, temporal_layers)?)
            }
            (SourceKind::Video(VideoInput::EncodedH264), InputFrame::H264Simulcast { frames })
                if self.encodings.len() == 3 =>
            {
                let frames: [H264Frame; 3] = frames
                    .try_into()
                    .map_err(|_| invalid("simulcast requires three genuine access units"))?;
                let [a, b, c] = frames;
                session.push_video_simulcast(
                    self.id,
                    [
                        video_unit(a, Some(0), 1)?,
                        video_unit(b, Some(1), 1)?,
                        video_unit(c, Some(2), 1)?,
                    ],
                )
            }
            _ => Err(invalid(
                "frame does not match the source's admitted input profile",
            )),
        }
    }

    pub fn close<R: rtc::SessionRuntime>(
        &mut self,
        session: &mut rtc::Session<R>,
    ) -> Result<(), rtc::PeerError> {
        match self.kind {
            SourceKind::Audio(_) => session.replace_audio_source(self.sender, None, "")?,
            SourceKind::Video(_) => session.replace_video_source(self.sender, None, "")?,
        }
        session.close_source(self.id)?;
        self.active = false;
        Ok(())
    }
}

fn video_unit(
    frame: H264Frame,
    simulcast_index: Option<u8>,
    temporal_layers: u8,
) -> Result<rtc::EncodedVideoAccessUnit, rtc::PeerError> {
    if temporal_layers == 3 && !frame.temporal_index.is_some_and(|index| index < 3) {
        return Err(invalid("L1T3 requires explicit temporal indices 0..=2"));
    }
    if temporal_layers == 1 && frame.temporal_index.is_some_and(|index| index != 0) {
        return Err(invalid("L1T1 does not admit enhancement temporal layers"));
    }
    Ok(rtc::EncodedVideoAccessUnit {
        data: frame.payload,
        width: u32::from(frame.width),
        height: u32::from(frame.height),
        timestamp_us: frame.timestamp_us,
        key_frame: frame.keyframe,
        qp: None,
        metadata: rtc::EncodedVideoMetadata {
            // The producer declares its H.264 dependency structure. The agent
            // must not derive base-layer sync from a fabricated GOP schedule.
            codec: rtc::EncodedVideoCodec::H264 {
                base_layer_sync: frame.base_layer_sync,
            },
            simulcast_index,
            spatial_index: None,
            // Core accepts an explicit base layer for L1T1; the native single-
            // layer encoder represents that same stream without a temporal ID.
            temporal_index: if temporal_layers == 1 {
                None
            } else {
                frame.temporal_index
            },
            end_of_picture: true,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_media::{self, PcmFrame};
    use std::time::{Duration, Instant};

    fn source_shapes<R: rtc::SessionRuntime>(
        runtime: R,
        audio: AudioInput,
        video: VideoInput,
        mut progress: impl FnMut(),
    ) {
        let mut session = native_media::session(runtime, audio, video)
            .unwrap_or_else(|error| panic!("{audio:?}/{video:?} factory failed: {error:?}"));
        let peer = session
            .create_peer(rtc::PeerConfiguration::default())
            .unwrap();
        let mut audio_source = NativeSource::create(
            &mut session,
            peer,
            "audio",
            SourceKind::Audio(audio),
            &VideoSourceConfig::default(),
        )
        .unwrap();
        assert!(
            audio_source
                .push(
                    &mut session,
                    InputFrame::H264Simulcast { frames: Vec::new() }
                )
                .is_err()
        );
        if audio == AudioInput::Pcm {
            audio_source
                .push(
                    &mut session,
                    InputFrame::Pcm {
                        frame: PcmFrame {
                            sample_rate_hz: 48_000,
                            channels: 1,
                            samples: vec![0; 480],
                            timestamp_us: 0,
                        },
                    },
                )
                .unwrap();
        }
        assert!(!audio_source.set_enabled(&mut session, false).unwrap());
        assert!(audio_source.set_enabled(&mut session, true).unwrap());
        let old_source = audio_source.id;
        audio_source
            .replace(&mut session, "replacement-audio")
            .unwrap();
        assert_ne!(old_source, audio_source.id);
        assert!(
            session
                .push_pcm(
                    old_source,
                    &rtc::AudioPcmFrame::i16_interleaved(48_000, 1, 0, vec![0; 480]).unwrap()
                )
                .is_err()
        );
        for (index, config) in [
            VideoSourceConfig::default(),
            VideoSourceConfig {
                encodings: vec![Encoding {
                    temporal_layers: 3,
                    ..Encoding::default()
                }],
                ..VideoSourceConfig::default()
            },
            VideoSourceConfig {
                encodings: ["low", "middle", "high"]
                    .into_iter()
                    .map(|rid| Encoding {
                        rid: Some(rid.into()),
                        ..Encoding::default()
                    })
                    .collect(),
                ..VideoSourceConfig::default()
            },
        ]
        .into_iter()
        .enumerate()
        {
            let source = NativeSource::create(
                &mut session,
                peer,
                &format!("video-{index}"),
                SourceKind::Video(video),
                &config,
            )
            .unwrap();
            assert_eq!(
                session.transceiver_media_type(source.transceiver).unwrap(),
                rtc::MediaType::Video
            );
            let before = source.encoding_state(&mut session).unwrap();
            let invalid_limits = vec![
                crate::EncodingLimits {
                    max_bitrate_bps: None,
                    max_framerate: Some(-1.0)
                };
                before.len()
            ];
            assert!(source.set_limits(&mut session, &invalid_limits).is_err());
            assert_eq!(before, source.encoding_state(&mut session).unwrap());
            assert_eq!(
                source.feedback(&mut session).is_ok(),
                video == VideoInput::EncodedH264
            );
            assert!(
                source
                    .push(
                        &mut session,
                        InputFrame::H264Simulcast { frames: Vec::new() }
                    )
                    .is_err()
            );
        }
        let operation = session.create_offer(peer, false).unwrap();
        let mut offer = None;
        loop {
            while let Some(event) = session.try_peer_event(peer).unwrap() {
                if let rtc::SessionEvent::OperationComplete(completion) = event
                    && completion.operation_id == operation
                {
                    offer = completion.result.unwrap();
                }
            }
            if offer.is_some() {
                break;
            }
            progress();
        }
        let offer = offer.expect("native source offer completed");
        assert!(offer.sdp.contains("H264/90000"));
        assert!(offer.sdp.contains("opus/48000"));
        assert!(!offer.sdp.contains("VP8/90000"));
        audio_source.close(&mut session).unwrap();
        assert!(
            audio_source
                .push(
                    &mut session,
                    InputFrame::Pcm {
                        frame: PcmFrame {
                            sample_rate_hz: 48_000,
                            channels: 1,
                            samples: vec![0; 480],
                            timestamp_us: 0,
                        }
                    }
                )
                .is_err()
        );
        session.shutdown().unwrap();
    }

    // Constructor, SDP and ownership evidence only. This test does not claim
    // encoded payload interoperability, raw codec replay, or SFU forwarding.
    #[test]
    fn native_profiles_construct_through_the_same_source_api_in_both_runtimes() {
        let _guard = native_media::NATIVE_TEST_SERIAL
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (audio, video) in [
            (AudioInput::Pcm, VideoInput::I420),
            (AudioInput::Pcm, VideoInput::EncodedH264),
            (AudioInput::EncodedOpus { channels: 1 }, VideoInput::I420),
            (
                AudioInput::EncodedOpus { channels: 2 },
                VideoInput::EncodedH264,
            ),
        ] {
            let deadline = Instant::now() + Duration::from_secs(15);
            source_shapes(rtc::ThreadedRuntime, audio, video, || {
                assert!(Instant::now() < deadline, "native offer stalled");
                std::thread::yield_now();
            });
            let world = rtc::ControlledWorld::acquire(29, Duration::from_secs(3)).unwrap();
            let network = world.create_network().unwrap();
            let endpoint = network
                .register_endpoint("192.0.2.1".parse().unwrap())
                .unwrap();
            let runtime = world.runtime().unwrap().with_endpoint(&endpoint).unwrap();
            let mut turns = 0;
            source_shapes(runtime, audio, video, || {
                turns += 1;
                assert!(turns <= 4096, "managed source offer stalled");
                assert!(!world.pump(64).blocked);
            });
        }
    }

    #[test]
    fn unsupported_shapes_are_not_silently_downgraded() {
        assert!(
            encoding_values(&VideoSourceConfig {
                dependency_descriptor: false,
                encodings: vec![Encoding {
                    temporal_layers: 3,
                    ..Encoding::default()
                }],
            })
            .is_err()
        );
        for encodings in [
            Vec::new(),
            vec![Encoding::default(); 4],
            vec![Encoding {
                temporal_layers: 2,
                ..Encoding::default()
            }],
            vec![
                Encoding {
                    temporal_layers: 3,
                    ..Encoding::default()
                };
                3
            ],
        ] {
            assert!(
                encoding_values(&VideoSourceConfig {
                    encodings,
                    ..VideoSourceConfig::default()
                })
                .is_err()
            );
        }
        let unit = video_unit(
            H264Frame {
                timestamp_us: 1,
                width: 16,
                height: 16,
                keyframe: false,
                temporal_index: Some(2),
                base_layer_sync: false,
                payload: Vec::new(),
            },
            None,
            3,
        )
        .unwrap();
        assert_eq!(unit.metadata.temporal_index, Some(2));
        assert!(matches!(
            unit.metadata.codec,
            rtc::EncodedVideoCodec::H264 {
                base_layer_sync: false
            }
        ));
    }

    #[test]
    fn single_temporal_layer_uses_native_absent_temporal_index() {
        let frame = |temporal_index| H264Frame {
            timestamp_us: 1,
            width: 16,
            height: 16,
            keyframe: false,
            temporal_index,
            base_layer_sync: false,
            payload: Vec::new(),
        };
        for simulcast_index in [None, Some(0), Some(1), Some(2)] {
            for temporal_index in [None, Some(0)] {
                let unit = video_unit(frame(temporal_index), simulcast_index, 1).unwrap();
                assert_eq!(unit.metadata.temporal_index, None);
                assert_eq!(unit.metadata.simulcast_index, simulcast_index);
            }
            for temporal_index in [Some(1), Some(2)] {
                assert!(video_unit(frame(temporal_index), simulcast_index, 1).is_err());
            }
        }
    }
}
