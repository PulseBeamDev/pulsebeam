//! Owning native audio encoder products. No mutable native view escapes.

fn control_result(valid: bool) -> Result<(), AudioEncoderError> {
    if valid {
        Ok(())
    } else {
        Err(AudioEncoderError::InvalidControl)
    }
}

use crate::{Environment, ffi};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioEncoderError {
    /// Encode consumes exactly one 10 ms interleaved PCM block.
    InvalidSamples,
    /// The selected native encoder violated its owning output contract.
    NativeInvariant,
    /// A control would enter unsafe native arithmetic or an aborting codec path.
    InvalidControl,
    /// The product's actual environment lacks the required durable clock proof.
    UnsafeClock,
    /// The selected pinned encoder has no native implementation of this control.
    UnsupportedControl,
}
impl std::fmt::Display for AudioEncoderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidSamples => "audio encoder requires one exact 10 ms PCM block",
            Self::NativeInvariant => "native audio encoder violated its output contract",
            Self::InvalidControl => "audio encoder control violates native preconditions",
            Self::UnsafeClock => "audio encoder requires a bounded nondecreasing native clock",
            Self::UnsupportedControl => {
                "selected native audio encoder does not support this control"
            }
        })
    }
}
impl std::error::Error for AudioEncoderError {}

struct AudioEncoderInner {
    native: cxx::UniquePtr<ffi::NativeAudioEncoder>,
    // Native environment ownership alone does not retain Rust randomness leases.
    _environment: Environment,
}
// SAFETY: native getters and mutations share one product mutex; storage and
// environment remain owned until the last handle is dropped.
unsafe impl Send for AudioEncoderInner {}
// SAFETY: no encoder, buffer or mutable environment reference escapes.
unsafe impl Sync for AudioEncoderInner {}

// One-shot factory handoff preparation. Native storage is dropped before the
// Rust environment lease, so backend destruction cannot outlive randomness.
// The eventual SDK adapter must forward through this exact guarded product,
// not detach its encoder or infer Opus provenance from the requested format.
pub(crate) struct AudioEncoderTransfer {
    pub(crate) native: cxx::UniquePtr<ffi::NativeAudioEncoder>,
    pub(crate) _environment: Environment,
}

// SAFETY: the complete original product/environment remains owned; SDK shared
// owners expose only const Rust borrows and all backend calls use its mutex.
unsafe impl Send for AudioEncoderTransfer {}
unsafe impl Sync for AudioEncoderTransfer {}
pub(crate) fn audio_encoder_transfer_native(
    source: &AudioEncoderTransfer,
) -> &ffi::NativeAudioEncoder {
    source.native.as_ref().expect("owned encoder handoff")
}

/// A live owning encoder. Clones name the same serialized native product.
#[derive(Clone)]
pub struct AudioEncoder(Arc<AudioEncoderInner>);
impl AudioEncoder {
    pub(crate) fn from_native(
        native: cxx::UniquePtr<ffi::NativeAudioEncoder>,
        environment: Environment,
    ) -> Self {
        assert!(!native.is_null());
        Self(Arc::new(AudioEncoderInner {
            native,
            _environment: environment,
        }))
    }
    pub(crate) fn try_into_factory_transfer(self) -> Result<AudioEncoderTransfer, Self> {
        match Arc::try_unwrap(self.0) {
            Ok(inner) => Ok(AudioEncoderTransfer {
                native: inner.native,
                _environment: inner._environment,
            }),
            Err(inner) => Err(Self(inner)),
        }
    }
    fn native(&self) -> &ffi::NativeAudioEncoder {
        self.0.native.as_ref().expect("validated audio encoder")
    }
    /// First rejected SDK forwarding control, independently retained after
    /// product retirement. Direct Rust controls report their own Result errors.
    pub fn sdk_failure_observer(&self) -> crate::AudioEncoderSdkFailureObserver {
        crate::AudioEncoderSdkFailureObserver::from_native(ffi::audio_encoder_sdk_failure_observer(
            self.native(),
        ))
    }
    pub fn sample_rate_hz(&self) -> i32 {
        ffi::audio_encoder_info(self.native()).sample_rate_hz
    }
    pub fn channels(&self) -> usize {
        ffi::audio_encoder_info(self.native()).num_channels
    }
    pub fn rtp_timestamp_rate_hz(&self) -> i32 {
        ffi::audio_encoder_info(self.native()).rtp_timestamp_rate_hz
    }
    pub fn frames_in_next_packet(&self) -> usize {
        ffi::audio_encoder_info(self.native()).next_packet_frames
    }
    pub fn maximum_frames_in_packet(&self) -> usize {
        ffi::audio_encoder_info(self.native()).maximum_packet_frames
    }
    pub fn target_bitrate_bps(&self) -> i32 {
        ffi::audio_encoder_info(self.native()).target_bitrate_bps
    }
    pub fn dtx(&self) -> bool {
        ffi::audio_encoder_info(self.native()).dtx
    }
    /// Consume one interleaved 10 ms block. Empty output while accumulating a
    /// longer packet is successful; timestamp and metadata are not synthesized.
    pub fn encode(
        &self,
        rtp_timestamp: u32,
        audio: &[i16],
    ) -> Result<crate::AudioEncoderOutput, AudioEncoderError> {
        let result = ffi::audio_encoder_encode(self.native(), rtp_timestamp, audio);
        match result.status {
            1 => return Err(AudioEncoderError::InvalidSamples),
            0 => {}
            _ => return Err(AudioEncoderError::NativeInvariant),
        }
        Ok(crate::AudioEncoderOutput {
            bytes: result.bytes,
            info: crate::AudioEncodedInfo {
                primary: crate::AudioEncodedInfoLeaf::from_native(result.primary),
                redundant: result
                    .redundant
                    .into_iter()
                    .map(crate::AudioEncodedInfoLeaf::from_native)
                    .collect(),
            },
        })
    }
    pub fn reset(&self) {
        ffi::audio_encoder_reset(self.native());
    }
    pub fn set_fec(&self, enable: bool) -> bool {
        ffi::audio_encoder_set_fec(self.native(), enable)
    }
    pub fn set_dtx(&self, enable: bool) -> bool {
        ffi::audio_encoder_set_dtx(self.native(), enable)
    }
    pub fn set_application(&self, application: crate::AudioEncoderApplication) -> bool {
        ffi::audio_encoder_set_application(
            self.native(),
            application == crate::AudioEncoderApplication::Speech,
        )
    }
    pub fn set_max_playback_rate(&self, frequency_hz: i32) {
        ffi::audio_encoder_set_max_playback_rate(self.native(), frequency_hz);
    }
    pub fn set_target_bitrate(&self, bitrate_bps: i32) -> Result<(), AudioEncoderError> {
        control_result(ffi::audio_encoder_set_target_bitrate(
            self.native(),
            bitrate_bps,
        ))
    }
    pub fn on_received_target_audio_bitrate(
        &self,
        bitrate_bps: i32,
    ) -> Result<(), AudioEncoderError> {
        control_result(ffi::audio_encoder_on_target_bitrate(
            self.native(),
            bitrate_bps,
        ))
    }
    pub fn on_received_uplink_allocation(
        &self,
        update: crate::BitrateAllocationUpdate,
    ) -> Result<(), AudioEncoderError> {
        control_result(ffi::audio_encoder_on_allocation(
            self.native(),
            &update.to_native(),
        ))
    }
    pub fn on_received_overhead(&self, bytes_per_packet: usize) -> Result<(), AudioEncoderError> {
        control_result(ffi::audio_encoder_on_overhead(
            self.native(),
            bytes_per_packet,
        ))
    }
    /// Ordinary Opus requires a finite fraction in 0..=1 and a retaining
    /// nondecreasing clock whose complete range fits native timestamp subtraction.
    pub fn on_received_uplink_packet_loss_fraction(
        &self,
        fraction: f32,
    ) -> Result<(), AudioEncoderError> {
        match ffi::audio_encoder_on_packet_loss(self.native(), fraction) {
            0 => Ok(()),
            1 => Err(AudioEncoderError::InvalidControl),
            2 => Err(AudioEncoderError::UnsafeClock),
            _ => Err(AudioEncoderError::NativeInvariant),
        }
    }
    /// No selected pinned implementation overrides the aborting native default.
    /// Report that unsupported control rather than invoking DCHECK_NOTREACHED.
    pub fn on_received_uplink_recoverable_packet_loss_fraction(
        &self,
        _fraction: f32,
    ) -> Result<(), AudioEncoderError> {
        Err(AudioEncoderError::UnsupportedControl)
    }
    pub fn on_received_rtt(&self, rtt_ms: i32) -> Result<(), AudioEncoderError> {
        control_result(ffi::audio_encoder_on_rtt(self.native(), rtt_ms))
    }
    /// Forward the frame-length range to the selected stock encoder.
    pub fn set_receiver_frame_length_range(
        &self,
        minimum_ms: i32,
        maximum_ms: i32,
    ) -> Result<(), AudioEncoderError> {
        control_result(ffi::audio_encoder_set_receiver_frame_length_range(
            self.native(),
            minimum_ms,
            maximum_ms,
        ))
    }
    pub fn frame_length_range(&self) -> Option<crate::AudioEncoderFrameLengthRange> {
        let range = ffi::audio_encoder_frame_length_range(self.native());
        range.present.then(|| crate::AudioEncoderFrameLengthRange {
            minimum: crate::TimeDelta::from_native_microsecond_bits(range.minimum),
            maximum: crate::TimeDelta::from_native_microsecond_bits(range.maximum),
        })
    }
    pub fn bitrate_range(&self) -> Option<crate::AudioEncoderBitrateRange> {
        let range = ffi::audio_encoder_bitrate_range(self.native());
        range.present.then(|| crate::AudioEncoderBitrateRange {
            minimum: crate::DataRate::from_native(range.minimum, range.minimum_kind),
            maximum: crate::DataRate::from_native(range.maximum, range.maximum_kind),
        })
    }
    pub fn network_adaptation_statistics(&self) -> crate::AudioNetworkAdaptationStatistics {
        let statistics = ffi::audio_encoder_statistics(self.native());
        let counter = |i: usize| {
            (statistics.present_mask & (1u8 << i) != 0).then_some(statistics.counters[i])
        };
        crate::AudioNetworkAdaptationStatistics {
            bitrate_action_counter: counter(0),
            channel_action_counter: counter(1),
            dtx_action_counter: counter(2),
            fec_action_counter: counter(3),
            frame_length_increase_counter: counter(4),
            frame_length_decrease_counter: counter(5),
            uplink_packet_loss_fraction: statistics.loss_present.then_some(statistics.loss),
        }
    }
}
