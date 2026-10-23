#![allow(
    clippy::disallowed_types,
    reason = "bounded fixture-only capture shared with native encoder callbacks, never SFU shard state"
)]

use crate::*;
use std::sync::{Arc, Mutex};

type FixtureResult<T> = Result<T, Box<dyn std::error::Error>>;

/// Application-owned access unit produced by the pinned stock encoder. The
/// temporal fields are copied from its callback, not assigned from a cadence.
#[derive(Clone, Debug)]
pub struct NativeH264AccessUnit {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub rtp_timestamp: u32,
    pub keyframe: bool,
    pub temporal_index: Option<u8>,
    pub base_layer_sync: bool,
}

struct Capture(Arc<Mutex<Vec<NativeH264AccessUnit>>>);
impl EncodedImageCallbackProvider for Capture {
    fn encoded(
        &self,
        image: EncodedImage,
        info: Option<OwnedCodecSpecificInfo>,
    ) -> EncodedImageCallbackResult {
        let metadata = image.metadata();
        let temporal = info.and_then(|info| match info.fields().codec_specific {
            Some(CodecSpecificParameters::H264 {
                temporal_idx,
                base_layer_sync,
                ..
            }) => Some((temporal_idx, base_layer_sync)),
            _ => None,
        });
        self.0.lock().unwrap().push(NativeH264AccessUnit {
            data: image.to_vec(),
            width: metadata.width,
            height: metadata.height,
            rtp_timestamp: metadata.rtp_timestamp,
            keyframe: image.is_key(),
            temporal_index: temporal.and_then(|(index, _)| (index != u8::MAX).then_some(index)),
            base_layer_sync: temporal.is_some_and(|(_, sync)| sync),
        });
        EncodedImageCallbackResult {
            error: EncodedImageCallbackError::Ok,
            frame_id: 0,
            drop_next_frame: false,
        }
    }
    fn frame_dropped(&self, _: u32, _: i32, _: bool) {}
}

/// Generate an elementary-stream fixture without an extra peer, alternate
/// encoder or threaded SDK environment. Release the product in this scope on
/// the world's thread. Returned values have no native ownership.
pub fn stock_h264_fixture(
    world: &SimulationWorld,
    temporal_layers: u8,
    frames: u32,
) -> FixtureResult<Vec<NativeH264AccessUnit>> {
    if !matches!(temporal_layers, 1..=3) || frames == 0 || frames > 120 {
        return Err(std::io::Error::other(
            "bounded H264 fixture needs 1..=3 layers and 1..=120 frames",
        )
        .into());
    }
    let runtime = world.controlled.runtime()?;
    let environment_factory = runtime.environment_factory()?;
    let environment = runtime.create_environment(&environment_factory)?;
    let factory = VideoEncoderFactoryHandle::builtin()?;
    let format = SdpVideoFormat::from(
        &VideoCodecFormat::new("H264")
            .with_parameter("packetization-mode", "1")
            .with_parameter("profile-level-id", "42e01f"),
    );
    let mut encoder = factory.create_encoder(&environment, &format)?;
    let output = Arc::new(Mutex::new(Vec::new()));
    let callback = EncodedImageCallbackSink::new(Capture(output.clone()));
    check(
        encoder.register_callback(Some(&callback)),
        "register callback",
    )?;
    let mut codec = OwnedVideoCodec::default();
    let mut fields = codec.fields();
    fields.codec_type = VideoCodecType::H264;
    fields.width = 320;
    fields.height = 180;
    fields.max_framerate = 30;
    fields.start_bitrate_kbps = 500;
    fields.min_bitrate_kbps = 100;
    fields.max_bitrate_kbps = 1_000;
    fields.codec_specific = Some(VideoCodecSpecificConfiguration::H264 {
        key_frame_interval: 120,
        temporal_layers,
    });
    codec.set_fields(&fields)?;
    check(
        encoder.initialize(
            &codec,
            VideoEncoderSettingsValue {
                loss_notification: false,
                number_of_cores: 1,
                max_payload_size: 1200,
                encoder_thread_limit: None,
            },
        ),
        "initialize stock H264",
    )?;
    let mut allocation = OwnedVideoBitrateAllocation::default();
    for layer in 0..temporal_layers {
        allocation.set_bitrate(0, usize::from(layer), 500_000 / u32::from(temporal_layers))?;
    }
    let rates = OwnedVideoRateControl::new(allocation.fields(), 30.0, None)?;
    encoder.set_rates(&rates);
    for index in 0..frames {
        let mut pixels = vec![96; 320 * 180 * 3 / 2];
        for y in 0..180 {
            for x in 0..320 {
                pixels[y * 320 + x] = if (x / 32 + y / 30 + usize::try_from(index / 8)?) % 2 == 0 {
                    48
                } else {
                    176
                };
            }
        }
        let legacy = VideoFrame::i420(320, 180, pixels, i64::from(index) * 33_333, index * 3_000)?;
        let frame = RawVideoFrame::from_legacy(&legacy)?;
        let kind = if index == 0 {
            EncodedImageFrameType::Key
        } else {
            EncodedImageFrameType::Delta
        };
        check(encoder.encode(&frame, Some(&[kind])), "encode stock H264")?;
        world.pump();
    }
    check(encoder.register_callback(None), "unregister callback")?;
    let units = std::mem::take(&mut *output.lock().unwrap());
    if units.is_empty() {
        return Err(std::io::Error::other("stock H264 produced no access units").into());
    }
    Ok(units)
}

fn check(status: i32, operation: &str) -> FixtureResult<()> {
    if status == 0 {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("{operation}: native status {status}")).into())
    }
}
