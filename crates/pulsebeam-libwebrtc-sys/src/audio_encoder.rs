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
    /// An ANA configuration or reached consumer cannot be safely admitted.
    UnsafeAdaptor,
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
            Self::UnsafeAdaptor => {
                "audio network adaptor configuration or consumer violates admission"
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
            3 => return Err(AudioEncoderError::UnsafeAdaptor),
            4 => return Err(AudioEncoderError::UnsafeClock),
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
    /// Enable/replace actual native ANA using the supplied protobuf bytes.
    /// Admission failure does not invoke replacement. A native false remains
    /// false, not a fabricated capability result. Reset preserves ANA history;
    /// disable/re-enable replaces controllers but retains the bitrate smoother.
    pub fn enable_audio_network_adaptor(&self, config: &[u8]) -> Result<bool, AudioEncoderError> {
        let mut error = 0;
        let enabled = ffi::audio_encoder_enable_network_adaptor(self.native(), config, &mut error);
        match error {
            0 => Ok(enabled),
            1 => Err(AudioEncoderError::InvalidControl),
            2 => Err(AudioEncoderError::UnsafeClock),
            3 => Err(AudioEncoderError::UnsupportedControl),
            4 => Err(AudioEncoderError::UnsafeAdaptor),
            _ => Err(AudioEncoderError::NativeInvariant),
        }
    }
    /// Delegate the native idempotent adaptor teardown.
    pub fn disable_audio_network_adaptor(&self) {
        ffi::audio_encoder_disable_network_adaptor(self.native());
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
    pub fn on_received_uplink_bandwidth(
        &self,
        bitrate_bps: i32,
        bwe_period_ms: Option<i64>,
    ) -> Result<(), AudioEncoderError> {
        control_result(ffi::audio_encoder_on_bandwidth(
            self.native(),
            bitrate_bps,
            bwe_period_ms.is_some(),
            bwe_period_ms.unwrap_or(0),
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
            3 => Err(AudioEncoderError::UnsafeAdaptor),
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
    /// Refuse the native ordering DCHECK in asserting profiles while ANA is
    /// active. Nonasserting profiles retain native behavior; no adaptor rebuild
    /// or silent teardown is substituted for the operation.
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

#[cfg(test)]
mod adaptor_admission_tests {
    use super::*;
    use crate::{AudioEncoderFactory, AudioEncoderOptions, SdpAudioFormat};

    fn controller_config(tag: u16, body: &[u8]) -> Vec<u8> {
        assert!((21..=27).contains(&tag));
        assert!(body.len() < 125);
        let key = (tag << 3) | 2;
        let mut bytes = vec![
            0x0a,
            (body.len() + 3) as u8,
            (key as u8 & 0x7f) | 0x80,
            (key >> 7) as u8,
            body.len() as u8,
        ];
        bytes.extend_from_slice(body);
        bytes
    }

    fn encoder_in(
        environment: &Environment,
        format: &str,
        rate: i32,
        channels: usize,
    ) -> AudioEncoder {
        AudioEncoderFactory::builtin()
            .unwrap()
            .create(
                environment,
                &SdpAudioFormat::new(format.as_bytes().to_vec(), rate, channels),
                &AudioEncoderOptions {
                    payload_type: 111,
                    codec_pair_id: None,
                },
            )
            .unwrap()
            .unwrap()
    }

    fn encoder(format: &str, rate: i32, channels: usize) -> AudioEncoder {
        encoder_in(
            &Environment::builder().build().unwrap(),
            format,
            rate,
            channels,
        )
    }

    #[test]
    fn preflight_does_not_construct_or_mutate_an_adaptor() {
        let encoder = encoder("opus", 48000, 2);
        let original = (
            encoder.target_bitrate_bps(),
            encoder.frames_in_next_packet(),
            encoder.dtx(),
        );
        let check =
            |bytes: &[u8]| ffi::audio_encoder_check_network_adaptor_config(encoder.native(), bytes);
        // ControllerManager.controllers = DtxController { enabling=1, disabling=2 }.
        let dtx = [0x0a, 7, 0xc2, 1, 4, 8, 1, 16, 2];
        let availability = check(&[]);
        assert!(matches!(availability, 0 | 3));
        if availability == 3 {
            assert_eq!(check(&dtx), 3);
            assert_eq!(check(&[0xff]), 3);
        } else {
            assert_eq!(check(&dtx), 0);
            assert_eq!(check(&[0xff]), 1); // Malformed protobuf.
            assert_eq!(check(&[0x0a, 0]), 1); // Unset controller oneof.
            assert_eq!(check(&[0x0a, 3, 0xc2, 1, 0]), 1); // Missing DTX fields.
            assert_eq!(check(&[0x0a, 3, 0xd2, 1, 0]), 1); // Removed RPLR controller.
            assert_eq!(check(&[0x0a, 3, 0xaa, 1, 0]), 1); // Missing FEC fields (tag 21).
            assert_eq!(check(&[0x0a, 3, 0xba, 1, 0]), 1); // Missing channel fields (tag 23).
            assert_eq!(check(&[0x0a, 3, 0xb2, 1, 0]), 1); // Missing legacy frame fields (tag 22).
            let duplicate = [dtx.as_slice(), dtx.as_slice()].concat();
            assert_eq!(check(&duplicate), 0); // Writer occupancy is a reached decision check, not construction.
            // V2's absent optional fields natively mean zero/false.
            assert_eq!(check(&[0x0a, 3, 0xda, 1, 0]), 0); // V2 tag 27.
            // Ignored/unknown root fields are not rewritten or rejected.
            assert_eq!(check(&[0x78, 1]), 0);
        }
        assert_eq!(
            (
                encoder.target_bitrate_bps(),
                encoder.frames_in_next_packet(),
                encoder.dtx()
            ),
            original
        );
        assert!(
            encoder
                .network_adaptation_statistics()
                .bitrate_action_counter
                .is_none()
        );
    }

    #[test]
    fn non_opus_products_do_not_enter_the_controller_factory() {
        let encoder = encoder("PCMU", 8000, 1);
        assert_eq!(
            ffi::audio_encoder_check_network_adaptor_config(encoder.native(), &[]),
            3
        );
        assert_eq!(
            ffi::audio_encoder_check_network_adaptor_config(encoder.native(), &[0xff]),
            3
        );
    }

    #[test]
    fn smoothing_preflight_uses_exact_float_and_native_period_boundaries() {
        let check = ffi::audio_encoder_adaptor_smoothing_inputs_safe;
        for sample in [i32::MIN, -1, 0, 6000, i32::MAX - 64] {
            assert!(check(sample, false, i64::MIN));
        }
        for sample in [i32::MAX - 63, i32::MAX] {
            assert!(!check(sample, false, 0));
        }
        assert!(check(6000, true, 0));
        assert!(check(6000, true, i64::MIN / 4)); // Multiplies safely, narrows to zero.
        assert!(!check(6000, true, i64::MIN / 4 - 1));
        assert!(!check(6000, true, i64::MAX / 4 + 1));
        assert!(!check(6000, true, i64::MAX / 4)); // Safe multiply, negative narrowed int.
        assert!(!check(6000, true, -1));
        assert!(check(6000, true, -(1_i64 << 23))); // Negative native duration, alpha rounds to one.
        assert!(check(6000, true, i64::from(i32::MAX) / 4));
        // Multiplication reaches 2^31, then the pinned i64-to-int conversion
        // yields INT32_MIN. Binary32 exp rounds this tiny positive exponent to
        // one, so the frozen-filter behavior remains natively supported.
        assert!(check(6000, true, i64::from(i32::MAX) / 4 + 1));
        assert!(check(6000, true, 1_i64 << 30)); // Native narrowing wraps to zero.
    }

    #[test]
    fn initialization_witness_checks_the_consumed_rounded_calculations() {
        let check = ffi::audio_encoder_adaptor_initialization_safe;
        for duration in [0, 1, 2, 5000] {
            assert!(check(duration, u64::MAX));
        }
        assert!(!check(i32::MAX, u64::MAX)); // Rounded zero initialization divisor.
        assert!(!check(-1, 1)); // Amplifying post-initialization alpha.
        assert!(check(-1, 0)); // Every consumed post-initialization exponent is zero.
        assert!(check(i32::MIN, u64::MAX)); // Negative duration, rounded unit alpha.
    }

    #[test]
    fn scoring_preflight_checks_comparator_equivalence_without_rejecting_ignored_values() {
        let encoder = encoder("opus", 48000, 2);
        let check = |config: &[u8], bandwidth: Option<i32>, loss: Option<f32>| {
            ffi::audio_encoder_check_network_adaptor_scoring(
                encoder.native(),
                config,
                bandwidth.is_some(),
                bandwidth.unwrap_or_default(),
                loss.is_some(),
                loss.unwrap_or_default(),
            )
        };
        if check(&[], None, None) == 3 {
            return;
        } // Protobuf unavailable.
        let scored = |tag, body: &[u8], loss: f32| {
            let mut controller = controller_config(tag, body);
            controller[1] += 9;
            controller.extend_from_slice(&[0x0a, 7, 8, 0, 0x15]); // ScoringPoint { bandwidth=0, loss }.
            controller.extend_from_slice(&loss.to_le_bytes());
            controller
        };
        let config = |controllers: &[Vec<u8>], threshold: f32| {
            let mut bytes = controllers.concat();
            bytes.extend_from_slice(&[0x10, 0, 0x1d]); // Required scoring gate fields.
            bytes.extend_from_slice(&threshold.to_le_bytes());
            bytes
        };
        let nan = scored(25, &[], f32::NAN);
        let origin = scored(24, &[8, 1, 16, 2], 0.0);
        let equal = scored(23, &[8, 2, 16, 1], 0.0);
        let other = scored(23, &[8, 2, 16, 1], 1.0);
        // NaN is equivalent to every scored controller. One numeric class is
        // still strict weak, but two different numeric classes are not.
        assert_eq!(
            check(
                &config(&[nan.clone(), origin.clone()], 0.0),
                Some(0),
                Some(0.0)
            ),
            0
        );
        assert_eq!(
            check(
                &config(&[nan.clone(), origin.clone(), equal], 0.0),
                Some(0),
                Some(0.0)
            ),
            0
        );
        for threshold in [0.0, f32::NAN, f32::NEG_INFINITY] {
            let unsafe_order = config(&[nan.clone(), origin.clone(), other.clone()], threshold);
            assert_eq!(check(&unsafe_order, Some(0), Some(0.0)), 2);
            assert_eq!(check(&unsafe_order, None, Some(0.0)), 0);
            assert_eq!(check(&unsafe_order, Some(0), None), 0);
        }
        let clipped = scored(24, &[8, 1, 16, 2], f32::INFINITY);
        assert_eq!(
            check(
                &config(&[nan.clone(), clipped, other], f32::NAN),
                Some(0),
                Some(0.0)
            ),
            0
        );
        let infinite_distance = scored(23, &[8, 2, 16, 1], f32::NEG_INFINITY);
        assert_eq!(
            check(
                &config(&[nan, origin, infinite_distance], 0.0),
                Some(0),
                Some(0.0)
            ),
            2
        );
        assert_eq!(check(&[], Some(i32::MIN), Some(f32::NAN)), 0); // No scoring arithmetic.
        assert_eq!(check(&[0xff], None, None), 1);
        assert!(
            encoder
                .network_adaptation_statistics()
                .bitrate_action_counter
                .is_none()
        );
    }

    #[test]
    fn native_bitrate_guard_consumes_only_active_offsets_and_frame_history() {
        let step = |target: Option<i32>,
                    overhead: Option<usize>,
                    frame: Option<i32>,
                    increasing,
                    occupied| {
            ffi::FfiAudioEncoderBitrateStep {
                has_target: target.is_some(),
                target: target.unwrap_or_default(),
                has_overhead: overhead.is_some(),
                overhead: overhead.unwrap_or_default(),
                has_frame: frame.is_some(),
                frame: frame.unwrap_or_default(),
                increasing,
                occupied,
            }
        };
        let probe = |increase, decrease, steps: &[ffi::FfiAudioEncoderBitrateStep]| {
            ffi::audio_encoder_adaptor_bitrate_probe(20, increase, decrease, steps)
        };
        // Ignore frame slots before both retained metrics are known; retain a
        // later frame only when native MakeDecision actually consumes it.
        let sparse = probe(
            0,
            0,
            &[
                step(None, None, Some(0), false, false),
                step(Some(8000), Some(10), None, false, false),
                step(None, None, Some(40), false, false),
                step(None, None, None, false, false),
            ],
        );
        assert_eq!(sparse.status, 0);
        assert_eq!(sparse.bitrates, [6000, 4000, 6000, 6000]);
        let inactive = probe(
            i32::MIN,
            0,
            &[step(Some(6000), Some(8), None, false, false)],
        );
        assert_eq!(inactive.status, 0);
        assert_eq!(inactive.bitrates, [2800]);
        let positive = probe(1, i32::MIN, &[step(Some(6000), Some(8), None, true, false)]);
        assert_eq!(positive.status, 0);
        assert_eq!(positive.bitrates, [2400]);
        let asserting = crate::AudioCodecInfo::new(0, 0, 0).is_err();
        let selected_min = probe(i32::MIN, 0, &[step(Some(6000), Some(8), None, true, false)]);
        let rate = 8_usize
            .wrapping_add_signed(i32::MIN as isize)
            .wrapping_mul(8)
            .wrapping_mul(1000)
            / 20;
        let difference = 6000_i64 - i64::from(rate as i32);
        if asserting || i32::try_from(difference).is_err() {
            assert_eq!(selected_min.status, 3);
            assert!(selected_min.bitrates.is_empty());
        } else {
            assert_eq!(selected_min.status, 0);
            assert_eq!(selected_min.bitrates, [difference.max(0) as i32]);
        }
        let zero_overhead = probe(0, 0, &[step(None, Some(0), None, false, false)]);
        assert_eq!(zero_overhead.status, if asserting { 1 } else { 0 });
        let occupied = probe(0, 0, &[step(None, None, None, false, true)]);
        assert_eq!(occupied.status, if asserting { 2 } else { 0 });
        let consumed_zero_frame = probe(0, 0, &[step(Some(8000), Some(10), Some(0), false, false)]);
        assert_eq!(consumed_zero_frame.status, 3);
        let underflow = probe(
            0,
            0,
            &[step(Some(i32::MIN), Some(1), Some(1), false, false)],
        );
        assert_eq!(underflow.status, 3);
        // The SDK narrows this unsigned rate to a negative int. Preserve that
        // pinned conversion and safe subtraction, rather than saturating it.
        let wrapped = probe(0, 0, &[step(Some(0), Some(268436), Some(1), false, false)]);
        assert_eq!(wrapped.status, 0);
        assert_eq!(wrapped.bitrates, [2_147_479_296]);
        let construction = ffi::audio_encoder_adaptor_bitrate_probe(0, 0, 0, &[]);
        assert_eq!(construction.status, 4);
    }

    #[test]
    fn singleton_bounds_prove_frozen_packet_loss_time_without_a_monotonic_flag() {
        use crate::{Clock, ClockProvider, ClockTimeBounds, FiniteTimestamp, NtpTime, Timestamp};
        struct Frozen(FiniteTimestamp);
        impl ClockProvider for Frozen {
            fn current_time(&self) -> FiniteTimestamp {
                self.0
            }
            fn convert_timestamp_to_ntp_time(&self, _: Timestamp) -> NtpTime {
                NtpTime::from_raw(0)
            }
        }
        let epoch = FiniteTimestamp::from_microseconds(-1_230_000).unwrap();
        let bounds = ClockTimeBounds::new(epoch, epoch).unwrap();
        let clock = Clock::from_bounded_provider(Frozen(epoch), bounds);
        let environment = Environment::builder().clock_handle(&clock).build().unwrap();
        assert!(!environment.clock_is_monotonic());
        let encoder = encoder_in(&environment, "opus", 48000, 2);
        encoder
            .on_received_uplink_packet_loss_fraction(0.25)
            .unwrap();
        encoder
            .on_received_uplink_packet_loss_fraction(0.5)
            .unwrap();
        assert_eq!(
            clock.current_time().microseconds().unwrap(),
            epoch.microseconds()
        );
    }

    #[test]
    fn bounded_native_smoother_guards_actual_averages_and_ignored_periods() {
        use crate::{Clock, ClockProvider, ClockTimeBounds, FiniteTimestamp, NtpTime, Timestamp};
        struct FixedTime;
        impl ClockProvider for FixedTime {
            fn current_time(&self) -> FiniteTimestamp {
                FiniteTimestamp::from_microseconds(0).unwrap()
            }
            fn convert_timestamp_to_ntp_time(&self, _: Timestamp) -> NtpTime {
                NtpTime::from_raw(0)
            }
        }
        let make = |maximum_us| {
            let minimum = FiniteTimestamp::from_microseconds(0).unwrap();
            let maximum = FiniteTimestamp::from_microseconds(maximum_us).unwrap();
            let bounds = ClockTimeBounds::new(minimum, maximum).unwrap();
            let clock = Clock::from_monotonic_bounded_provider(FixedTime, bounds);
            let environment = Environment::builder().clock_handle(&clock).build().unwrap();
            encoder_in(&environment, "opus", 48000, 2)
        };
        let probe = |encoder: &AudioEncoder,
                     first,
                     sample,
                     sample_time,
                     average_time,
                     duration: Option<i32>| {
            ffi::audio_encoder_adaptor_smoothing_probe(
                encoder.native(),
                first,
                sample,
                0,
                sample_time,
                average_time,
                duration.is_some(),
                duration.unwrap_or_default(),
            )
        };
        let frozen = make(0);
        // Rounded-high samples are harmless while the actual native average
        // remains the first sample. The period is ignored during initialization.
        let ignored = probe(&frozen, 6000, i32::MAX, 0, 0, Some(-1));
        assert_eq!(ignored.status, 0);
        assert!(!ignored.duration_applied);
        assert!(ignored.has_average);
        assert_eq!(ignored.average, 6000.0);
        let lower = probe(&frozen, i32::MIN, i32::MAX, 0, 0, None);
        assert_eq!(lower.status, 0);
        assert_eq!(lower.average, -2147483648.0);
        // The exact same rounded-high value is unsafe when actually returned
        // as an average: block the SDK's int narrowing, not the sample itself.
        let high_average = probe(&frozen, i32::MAX, 6000, 0, 0, None);
        assert_eq!(high_average.status, 6);
        assert!(!high_average.has_average);

        let advancing = make(7_000_000);
        let immediate = probe(&advancing, 6000, i32::MAX, 6_000_000, 7_000_000, Some(0));
        assert!(immediate.duration_applied);
        assert_eq!(immediate.status, 6);
        assert!(!immediate.has_average);
        let wrapped_period = probe(&advancing, 6000, 5000, 6_000_000, 7_000_000, Some(i32::MIN));
        assert_eq!(wrapped_period.status, 0);
        assert!(wrapped_period.duration_applied);
        assert!(wrapped_period.has_average);
        assert_eq!(wrapped_period.average, 6000.0);
        let amplifying = probe(&advancing, 6000, 5000, 6_000_000, 6_001_000, Some(-1));
        assert_eq!(amplifying.status, 0);
        assert!(amplifying.duration_applied);
        assert!(amplifying.has_average);
        // Native alpha = exp(1), and time can advance. Admit the actual safe
        // average rather than rejecting a finite amplifying time constant.
        assert!((amplifying.average - 7718.282).abs() < 2.0);
        let overflow = probe(&advancing, 6000, 5000, 6_000_000, 7_000_000, Some(-1));
        assert!(overflow.duration_applied);
        assert_eq!(overflow.status, 6);
        assert!(!overflow.has_average);
        let endpoint = make(6_000_000);
        let frozen_after_init = probe(&endpoint, 6000, i32::MAX, 6_000_000, 6_000_000, Some(-1));
        assert_eq!(frozen_after_init.status, 0);
        assert!(frozen_after_init.duration_applied);
        assert_eq!(frozen_after_init.average, 6000.0);
        let backwards = probe(&advancing, 6000, 6000, 6_000_000, 5_000_000, None);
        assert_eq!(backwards.status, 1);
        let out_of_bounds = probe(&frozen, 6000, 6000, 1_000_000, 1_000_000, None);
        assert_eq!(out_of_bounds.status, 1);
        assert!(
            advancing
                .network_adaptation_statistics()
                .bitrate_action_counter
                .is_none()
        );
    }

    #[test]
    fn retained_clock_preflight_distinguishes_millisecond_and_microsecond_math() {
        use crate::{Clock, ClockProvider, ClockTimeBounds, FiniteTimestamp, NtpTime, Timestamp};
        use std::sync::{
            Arc,
            atomic::{AtomicI64, Ordering},
        };
        struct TestTime(Arc<AtomicI64>);
        impl ClockProvider for TestTime {
            fn current_time(&self) -> FiniteTimestamp {
                FiniteTimestamp::from_microseconds(self.0.load(Ordering::Relaxed)).unwrap()
            }
            fn convert_timestamp_to_ntp_time(&self, _: Timestamp) -> NtpTime {
                NtpTime::from_raw(0)
            }
        }
        let make = |time: FiniteTimestamp, bounds| {
            let time = Arc::new(AtomicI64::new(time.microseconds()));
            let clock = Clock::from_monotonic_bounded_provider(TestTime(time.clone()), bounds);
            let environment = Environment::builder().clock_handle(&clock).build().unwrap();
            (encoder_in(&environment, "opus", 48000, 2), time)
        };
        let zero = FiniteTimestamp::from_microseconds(0).unwrap();
        let (wide, _) = make(zero, ClockTimeBounds::all_finite());
        assert_eq!(
            wide.on_received_uplink_packet_loss_fraction(0.1),
            Err(AudioEncoderError::UnsafeClock)
        );
        let negative = FiniteTimestamp::from_microseconds(-1000).unwrap();
        let (frozen, _) = make(negative, ClockTimeBounds::new(negative, negative).unwrap());
        frozen.on_received_uplink_packet_loss_fraction(0.1).unwrap();
        let minimum = FiniteTimestamp::from_microseconds(-1).unwrap();
        let maximum = FiniteTimestamp::from_microseconds(i64::MAX - 1).unwrap();
        let (endpoint, time) = make(minimum, ClockTimeBounds::new(minimum, maximum).unwrap());
        // Exact INT64_MAX subtraction fits i64, but the SDK's finite
        // TimeDelta::Micros constructor rejects that infinity sentinel.
        assert_eq!(
            endpoint.on_received_uplink_packet_loss_fraction(0.1),
            Err(AudioEncoderError::UnsafeClock)
        );
        time.store(maximum.microseconds(), Ordering::Relaxed);
        assert_eq!(
            endpoint.on_received_uplink_packet_loss_fraction(0.2),
            Err(AudioEncoderError::UnsafeClock)
        );
        let (finite, time) = make(zero, ClockTimeBounds::new(zero, maximum).unwrap());
        finite.on_received_uplink_packet_loss_fraction(0.1).unwrap();
        time.store(maximum.microseconds(), Ordering::Relaxed);
        finite.on_received_uplink_packet_loss_fraction(0.2).unwrap();
        let lower = FiniteTimestamp::from_microseconds(-2).unwrap();
        let (too_wide, _) = make(lower, ClockTimeBounds::new(lower, maximum).unwrap());
        assert_eq!(
            too_wide.on_received_uplink_packet_loss_fraction(0.1),
            Err(AudioEncoderError::UnsafeClock)
        );
        let check = |encoder: &AudioEncoder, config: &[u8]| {
            ffi::audio_encoder_check_network_adaptor_smoothing(encoder.native(), config)
        };
        if check(&wide, &[]) == 3 {
            return;
        } // Protobuf unavailable, but packet-loss checks above still apply.
        assert_eq!(check(&wide, &[]), 0);
        assert_eq!(check(&frozen, &[]), 0);

        // Equal enabling/disabling curves; only the smoothing duration differs.
        let threshold = [8, 0, 0x15, 0, 0, 0, 0, 0x18, 1, 0x25, 0, 0, 0, 0];
        let fec = |duration: i32| {
            let mut body = vec![0x0a, threshold.len() as u8];
            body.extend_from_slice(&threshold);
            body.extend_from_slice(&[0x12, threshold.len() as u8]);
            body.extend_from_slice(&threshold);
            body.push(0x18);
            let mut value = duration as i64 as u64;
            while value >= 128 {
                body.push((value as u8 & 0x7f) | 0x80);
                value >>= 7;
            }
            body.push(value as u8);
            controller_config(21, &body)
        };
        assert_eq!(check(&frozen, &fec(-1)), 0);
        assert_eq!(check(&wide, &fec(-1)), 2);
        assert_eq!(check(&wide, &fec(i32::MIN)), 0);
        assert_eq!(check(&frozen, &fec(i32::MAX)), 2);
        assert_eq!(check(&frozen, &[0xff]), 1);
        assert!(
            frozen
                .network_adaptation_statistics()
                .bitrate_action_counter
                .is_none()
        );
    }

    #[test]
    fn controller_arithmetic_preflight_preserves_consumed_native_domains() {
        let encoder = encoder("opus", 48000, 2);
        let check = |config: &[u8],
                     overhead: Option<usize>,
                     target: Option<i32>,
                     uplink: Option<(i32, i32)>| {
            let (min, max) = uplink.unwrap_or_default();
            ffi::audio_encoder_check_network_adaptor_arithmetic(
                encoder.native(),
                config,
                overhead.is_some(),
                overhead.unwrap_or_default(),
                target.is_some(),
                target.unwrap_or_default(),
                uplink.is_some(),
                min,
                max,
            )
        };
        if check(&[], None, None, None) == 3 {
            return;
        } // Protobuf unavailable.
        assert_eq!(check(&[], None, None, None), 0);
        let bitrate = controller_config(25, &[]);
        assert_eq!(check(&bitrate, None, Some(i32::MIN), None), 0);
        assert_eq!(check(&bitrate, Some(0), None, None), 2); // Metrics assertion, even without target.
        assert_eq!(check(&bitrate, Some(1), Some(i32::MIN), None), 2);
        assert_eq!(check(&bitrate, Some(1), Some(i32::MIN + 400), None), 0);
        let positive_decrease_offset = controller_config(25, &[0x10, 1]);
        assert_eq!(
            check(
                &positive_decrease_offset,
                Some(1),
                Some(i32::MIN + 800),
                None
            ),
            0
        );
        assert_eq!(
            check(
                &positive_decrease_offset,
                Some(1),
                Some(i32::MIN + 799),
                None
            ),
            2
        );
        let unused_increase_offset = controller_config(25, &[0x08, 2]);
        assert_eq!(
            check(&unused_increase_offset, Some(1), Some(60_000), None),
            0
        );
        let v2 = controller_config(27, &[]);
        assert_eq!(check(&v2, Some(268_435), Some(i32::MAX), Some((0, 0))), 0);
        assert_eq!(check(&v2, Some(268_436), Some(i32::MAX), Some((0, 0))), 2);
        assert_eq!(check(&v2, Some(268_436), Some(i32::MAX), None), 0); // Native early return.
        assert_eq!(
            check(&v2, Some(1), Some(60_000), Some((i32::MIN, i32::MIN))),
            0
        );
        let slow_v2 = controller_config(27, &[0x10, 1]);
        assert_eq!(
            check(&slow_v2, Some(1), Some(60_000), Some((i32::MIN, i32::MIN))),
            2
        );
        assert_eq!(
            check(&slow_v2, Some(1), Some(60_000), Some((6000, 60_000))),
            0
        );
        #[cfg(target_pointer_width = "64")]
        assert_eq!(
            check(&v2, Some(1_usize << 32), Some(i32::MIN), Some((0, 0))),
            0
        );
        let legacy = controller_config(22, &[0x0d, 0, 0, 0, 0, 0x15, 0, 0, 0, 0, 0x18, 0]);
        assert_eq!(check(&legacy, Some(5_368_709), None, None), 0);
        assert_eq!(check(&legacy, Some(5_368_709), None, Some((0, 0))), 2); // Threshold sum overflow.
    }

    #[test]
    fn v2_preflight_uses_the_actual_receiver_frame_set() {
        let encoder = encoder("opus", 48000, 2);
        let v2 = [0x0a, 3, 0xda, 1, 0];
        let availability = ffi::audio_encoder_check_network_adaptor_config(encoder.native(), &v2);
        assert!(matches!(availability, 0 | 3));
        encoder
            .set_receiver_frame_length_range(i32::MAX, i32::MIN)
            .unwrap();
        assert_eq!(
            ffi::audio_encoder_check_network_adaptor_config(encoder.native(), &v2),
            if availability == 3 { 3 } else { 1 }
        );
    }
}
