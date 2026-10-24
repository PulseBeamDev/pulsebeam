//! Complete owning NetEq config, decoder-format and statistics values.
//! These values are not a NetEq factory or product binding.
use crate::{SdpAudioFormat, ffi};

/// Native configuration. Values remain representable independently of the
/// safety constraints of a future selected native NetEq constructor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetEqConfig {
    pub sample_rate_hz: i32,
    pub max_packets_in_buffer: usize,
    pub max_delay_ms: i32,
    pub min_delay_ms: i32,
    pub enable_fast_accelerate: bool,
    pub enable_muted_state: bool,
    pub enable_rtx_handling: bool,
    pub for_test_no_time_stretching: bool,
}
impl Default for NetEqConfig {
    fn default() -> Self {
        Self::from_native(ffi::neteq_config_default())
    }
}
impl NetEqConfig {
    /// SDK's own description, which intentionally omits max_delay_ms and
    /// for_test_no_time_stretching in this revision.
    pub fn native_description(&self) -> String {
        ffi::neteq_config_description(&self.to_native())
    }
    pub(crate) fn from_native(value: ffi::FfiNetEqConfig) -> Self {
        Self {
            sample_rate_hz: value.sample_rate_hz,
            max_packets_in_buffer: value.max_packets_in_buffer,
            max_delay_ms: value.max_delay_ms,
            min_delay_ms: value.min_delay_ms,
            enable_fast_accelerate: value.enable_fast_accelerate,
            enable_muted_state: value.enable_muted_state,
            enable_rtx_handling: value.enable_rtx_handling,
            for_test_no_time_stretching: value.for_test_no_time_stretching,
        }
    }
    pub(crate) fn to_native(&self) -> ffi::FfiNetEqConfig {
        ffi::FfiNetEqConfig {
            sample_rate_hz: self.sample_rate_hz,
            max_packets_in_buffer: self.max_packets_in_buffer,
            max_delay_ms: self.max_delay_ms,
            min_delay_ms: self.min_delay_ms,
            enable_fast_accelerate: self.enable_fast_accelerate,
            enable_muted_state: self.enable_muted_state,
            enable_rtx_handling: self.enable_rtx_handling,
            for_test_no_time_stretching: self.for_test_no_time_stretching,
        }
    }
}

/// Exact native integer tag, retaining unknown future values.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct NetEqReturnCode(pub i32);
impl NetEqReturnCode {
    pub const OK: Self = Self(0);
    pub const FAIL: Self = Self(-1);
}

/// Exact native integer tag, retaining unknown future values.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct NetEqOperation(pub i32);
impl NetEqOperation {
    pub const NORMAL: Self = Self(0);
    pub const MERGE: Self = Self(1);
    pub const EXPAND: Self = Self(2);
    pub const ACCELERATE: Self = Self(3);
    pub const FAST_ACCELERATE: Self = Self(4);
    pub const PREEMPTIVE_EXPAND: Self = Self(5);
    pub const RFC3389_CNG: Self = Self(6);
    pub const RFC3389_CNG_NO_PACKET: Self = Self(7);
    pub const CODEC_INTERNAL_CNG: Self = Self(8);
    pub const DTMF: Self = Self(9);
    pub const UNDEFINED: Self = Self(10);
}

/// Exact native integer tag, retaining unknown future values.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct NetEqMode(pub i32);
impl NetEqMode {
    pub const NORMAL: Self = Self(0);
    pub const EXPAND: Self = Self(1);
    pub const MERGE: Self = Self(2);
    pub const ACCELERATE_SUCCESS: Self = Self(3);
    pub const ACCELERATE_LOW_ENERGY: Self = Self(4);
    pub const ACCELERATE_FAIL: Self = Self(5);
    pub const PREEMPTIVE_EXPAND_SUCCESS: Self = Self(6);
    pub const PREEMPTIVE_EXPAND_LOW_ENERGY: Self = Self(7);
    pub const PREEMPTIVE_EXPAND_FAIL: Self = Self(8);
    pub const RFC3389_CNG: Self = Self(9);
    pub const CODEC_INTERNAL_CNG: Self = Self(10);
    pub const CODEC_PLC: Self = Self(11);
    pub const DTMF: Self = Self(12);
    pub const ERROR: Self = Self(13);
    pub const UNDEFINED: Self = Self(14);
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NetEqDecoderFormat {
    pub payload_type: i32,
    pub sample_rate_hz: i32,
    pub num_channels: i32,
    pub sdp_format: SdpAudioFormat,
}
impl NetEqDecoderFormat {
    pub(crate) fn from_native(value: ffi::FfiNetEqDecoderFormat) -> Self {
        Self {
            payload_type: value.payload_type,
            sample_rate_hz: value.sample_rate_hz,
            num_channels: value.num_channels,
            sdp_format: SdpAudioFormat::from_native(value.sdp_format),
        }
    }
    pub(crate) fn to_native(&self) -> ffi::FfiNetEqDecoderFormat {
        ffi::FfiNetEqDecoderFormat {
            payload_type: self.payload_type,
            sample_rate_hz: self.sample_rate_hz,
            num_channels: self.num_channels,
            sdp_format: self.sdp_format.to_native(),
        }
    }
}

// Native widths and zero defaults are preserved. Network rates stay raw Q14,
// jitter_peaks_found stays u16 rather than being coerced into bool. These
// conversions do not imply resetting or fetching any actual NetEq counters.
macro_rules! neteq_statistics {
    ($name:ident, $native:ident { $($field:ident: $ty:ty),* $(,)? }) => {
        #[derive(Clone, Debug, Default, Eq, PartialEq)]
        pub struct $name { $(pub $field: $ty,)* }
        impl $name {
            pub(crate) fn from_native(value: ffi::$native) -> Self {
                Self { $($field: value.$field,)* }
            }
            pub(crate) fn to_native(&self) -> ffi::$native {
                ffi::$native { $($field: self.$field,)* }
            }
        }
    };
}

neteq_statistics!(
    NetEqNetworkStatistics,
    FfiNetEqNetworkStatistics {
        current_buffer_size_ms: u16,
        preferred_buffer_size_ms: u16,
        jitter_peaks_found: u16,
        expand_rate: u16,
        speech_expand_rate: u16,
        preemptive_rate: u16,
        accelerate_rate: u16,
        secondary_decoded_rate: u16,
        secondary_discarded_rate: u16,
        mean_waiting_time_ms: i32,
        median_waiting_time_ms: i32,
        min_waiting_time_ms: i32,
        max_waiting_time_ms: i32,
    }
);

neteq_statistics!(
    NetEqLifetimeStatistics,
    FfiNetEqLifetimeStatistics {
        total_samples_received: u64,
        concealed_samples: u64,
        concealment_events: u64,
        jitter_buffer_delay_ms: u64,
        jitter_buffer_emitted_count: u64,
        jitter_buffer_target_delay_ms: u64,
        jitter_buffer_minimum_delay_ms: u64,
        inserted_samples_for_deceleration: u64,
        removed_samples_for_acceleration: u64,
        silent_concealed_samples: u64,
        fec_packets_received: u64,
        fec_packets_discarded: u64,
        packets_discarded: u64,
        delayed_packet_outage_samples: u64,
        delayed_packet_outage_events: u64,
        relative_packet_arrival_delay_ms: u64,
        jitter_buffer_packets_received: u64,
        generated_noise_samples: u64,
        total_processing_delay_us: u64,
        interruption_count: i32,
        total_interruption_duration_ms: i32,
    }
);

neteq_statistics!(
    NetEqOperationsAndState,
    FfiNetEqOperationsAndState {
        preemptive_samples: u64,
        accelerate_samples: u64,
        packet_buffer_flushes: u64,
        last_waiting_time_ms: u64,
        current_buffer_size_ms: u64,
        current_frame_size_ms: u64,
        next_packet_available: bool,
    }
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_config_conversion_preserves_unvalidated_values_and_all_flags() {
        let value = NetEqConfig {
            sample_rate_hz: i32::MIN,
            max_packets_in_buffer: usize::MAX,
            max_delay_ms: -37,
            min_delay_ms: i32::MAX,
            enable_fast_accelerate: true,
            enable_muted_state: false,
            enable_rtx_handling: true,
            for_test_no_time_stretching: false,
        };
        assert_eq!(
            NetEqConfig::from_native(ffi::neteq_config_round_trip(&value.to_native())),
            value
        );
        let inverted = NetEqConfig {
            enable_fast_accelerate: false,
            enable_muted_state: true,
            enable_rtx_handling: false,
            for_test_no_time_stretching: true,
            ..value
        };
        assert_eq!(
            NetEqConfig::from_native(ffi::neteq_config_round_trip(&inverted.to_native())),
            inverted
        );
    }

    #[test]
    fn native_statistics_conversion_does_not_permute_or_omit_fields() {
        let value = NetEqNetworkStatistics {
            current_buffer_size_ms: 1,
            preferred_buffer_size_ms: 2,
            jitter_peaks_found: 3,
            expand_rate: 4,
            speech_expand_rate: 5,
            preemptive_rate: 6,
            accelerate_rate: 7,
            secondary_decoded_rate: 8,
            secondary_discarded_rate: 9,
            mean_waiting_time_ms: -10,
            median_waiting_time_ms: -11,
            min_waiting_time_ms: -12,
            max_waiting_time_ms: -13,
        };
        assert_eq!(
            NetEqNetworkStatistics::from_native(ffi::neteq_network_statistics_round_trip(
                &value.to_native()
            )),
            value
        );
        let value = NetEqLifetimeStatistics {
            total_samples_received: u64::MAX - 1,
            concealed_samples: u64::MAX - 2,
            concealment_events: u64::MAX - 3,
            jitter_buffer_delay_ms: u64::MAX - 4,
            jitter_buffer_emitted_count: u64::MAX - 5,
            jitter_buffer_target_delay_ms: u64::MAX - 6,
            jitter_buffer_minimum_delay_ms: u64::MAX - 7,
            inserted_samples_for_deceleration: u64::MAX - 8,
            removed_samples_for_acceleration: u64::MAX - 9,
            silent_concealed_samples: u64::MAX - 10,
            fec_packets_received: u64::MAX - 11,
            fec_packets_discarded: u64::MAX - 12,
            packets_discarded: u64::MAX - 13,
            delayed_packet_outage_samples: u64::MAX - 14,
            delayed_packet_outage_events: u64::MAX - 15,
            relative_packet_arrival_delay_ms: u64::MAX - 16,
            jitter_buffer_packets_received: u64::MAX - 17,
            generated_noise_samples: u64::MAX - 18,
            total_processing_delay_us: u64::MAX - 19,
            interruption_count: i32::MIN + 20,
            total_interruption_duration_ms: i32::MIN + 21,
        };
        assert_eq!(
            NetEqLifetimeStatistics::from_native(ffi::neteq_lifetime_statistics_round_trip(
                &value.to_native()
            )),
            value
        );
        let value = NetEqOperationsAndState {
            preemptive_samples: u64::MAX - 1,
            accelerate_samples: u64::MAX - 2,
            packet_buffer_flushes: u64::MAX - 3,
            last_waiting_time_ms: u64::MAX - 4,
            current_buffer_size_ms: u64::MAX - 5,
            current_frame_size_ms: u64::MAX - 6,
            next_packet_available: false,
        };
        assert_eq!(
            NetEqOperationsAndState::from_native(ffi::neteq_operations_and_state_round_trip(
                &value.to_native()
            )),
            value
        );
    }

    #[test]
    fn net_eq_network_statistics_round_trips_full_widths() {
        let value = NetEqNetworkStatistics {
            current_buffer_size_ms: u16::MAX,
            preferred_buffer_size_ms: u16::MAX,
            jitter_peaks_found: u16::MAX,
            expand_rate: u16::MAX,
            speech_expand_rate: u16::MAX,
            preemptive_rate: u16::MAX,
            accelerate_rate: u16::MAX,
            secondary_decoded_rate: u16::MAX,
            secondary_discarded_rate: u16::MAX,
            mean_waiting_time_ms: i32::MIN,
            median_waiting_time_ms: i32::MIN,
            min_waiting_time_ms: i32::MIN,
            max_waiting_time_ms: i32::MIN,
        };
        assert_eq!(
            NetEqNetworkStatistics::from_native(value.to_native()),
            value
        );
        assert_eq!(
            NetEqNetworkStatistics::from_native(ffi::neteq_network_statistics_round_trip(
                &value.to_native()
            )),
            value
        );
    }

    #[test]
    fn net_eq_lifetime_statistics_round_trips_full_widths() {
        let value = NetEqLifetimeStatistics {
            total_samples_received: u64::MAX,
            concealed_samples: u64::MAX,
            concealment_events: u64::MAX,
            jitter_buffer_delay_ms: u64::MAX,
            jitter_buffer_emitted_count: u64::MAX,
            jitter_buffer_target_delay_ms: u64::MAX,
            jitter_buffer_minimum_delay_ms: u64::MAX,
            inserted_samples_for_deceleration: u64::MAX,
            removed_samples_for_acceleration: u64::MAX,
            silent_concealed_samples: u64::MAX,
            fec_packets_received: u64::MAX,
            fec_packets_discarded: u64::MAX,
            packets_discarded: u64::MAX,
            delayed_packet_outage_samples: u64::MAX,
            delayed_packet_outage_events: u64::MAX,
            relative_packet_arrival_delay_ms: u64::MAX,
            jitter_buffer_packets_received: u64::MAX,
            generated_noise_samples: u64::MAX,
            total_processing_delay_us: u64::MAX,
            interruption_count: i32::MIN,
            total_interruption_duration_ms: i32::MIN,
        };
        assert_eq!(
            NetEqLifetimeStatistics::from_native(value.to_native()),
            value
        );
        assert_eq!(
            NetEqLifetimeStatistics::from_native(ffi::neteq_lifetime_statistics_round_trip(
                &value.to_native()
            )),
            value
        );
    }

    #[test]
    fn net_eq_operations_and_state_round_trips_full_widths() {
        let value = NetEqOperationsAndState {
            preemptive_samples: u64::MAX,
            accelerate_samples: u64::MAX,
            packet_buffer_flushes: u64::MAX,
            last_waiting_time_ms: u64::MAX,
            current_buffer_size_ms: u64::MAX,
            current_frame_size_ms: u64::MAX,
            next_packet_available: true,
        };
        assert_eq!(
            NetEqOperationsAndState::from_native(value.to_native()),
            value
        );
        assert_eq!(
            NetEqOperationsAndState::from_native(ffi::neteq_operations_and_state_round_trip(
                &value.to_native()
            )),
            value
        );
    }

    #[test]
    fn stock_preconditions_do_not_restrict_value_round_trips() {
        let ceiling = (i32::MAX / (3 * 120)) as usize;
        for rate in [8000, 16000, 32000, 48000] {
            for count in [0, 1, ceiling] {
                let config = NetEqConfig {
                    sample_rate_hz: rate,
                    max_packets_in_buffer: count,
                    max_delay_ms: i32::MIN,
                    min_delay_ms: i32::MAX,
                    ..NetEqConfig::default()
                };
                assert!(ffi::neteq_stock_config_safe(&config.to_native()));
            }
        }
        for (rate, count) in [(44100, 1), (8000, ceiling + 1), (48000, usize::MAX)] {
            let config = NetEqConfig {
                sample_rate_hz: rate,
                max_packets_in_buffer: count,
                ..NetEqConfig::default()
            };
            assert!(!ffi::neteq_stock_config_safe(&config.to_native()));
            assert_eq!(
                NetEqConfig::from_native(ffi::neteq_config_round_trip(&config.to_native())),
                config
            );
        }
        for size in [0, 1, i32::MAX as usize] {
            assert!(ffi::neteq_stock_payload_size_safe(size));
        }
        assert!(!ffi::neteq_stock_payload_size_safe(i32::MAX as usize + 1));
        assert!(!ffi::neteq_stock_payload_size_safe(usize::MAX));
    }

    #[test]
    fn stock_red_preflight_retains_inner_types_and_rejects_truncated_lengths() {
        let mut types = vec![99];
        assert!(ffi::neteq_stock_red_payload_types(
            &[0xe0, 0, 0, 2, 97, 1, 2, 3],
            &mut types
        ));
        assert_eq!(types, [96, 97]);
        assert!(ffi::neteq_stock_red_payload_types(
            &[0xe0, 0, 0, 0, 97, 3],
            &mut types
        ));
        assert_eq!(types, [97]);
        assert!(ffi::neteq_stock_red_payload_types(&[97], &mut types));
        assert!(types.is_empty());
        for malformed in [
            &[][..],
            &[0xe0],
            &[0xe0, 0, 0],
            &[0xe0, 0, 0, 1],
            &[0xe0, 0, 0, 3, 97, 1, 2],
        ] {
            types.push(99);
            assert!(!ffi::neteq_stock_red_payload_types(malformed, &mut types));
            assert!(types.is_empty());
        }
        let mut blocks = Vec::new();
        for _ in 0..32 {
            blocks.extend_from_slice(&[0xe0, 0, 0, 1]);
        }
        blocks.push(97);
        blocks.extend_from_slice(&[1; 33]);
        assert!(!ffi::neteq_stock_red_payload_types(&blocks, &mut types));
        assert!(types.is_empty());
        blocks.pop(); // Empty primary leaves exactly 32 nonempty blocks.
        assert!(ffi::neteq_stock_red_payload_types(&blocks, &mut types));
        assert_eq!(types, [96; 32]);
        // Reject on the 33rd nonempty redundant block, even before a primary
        // header. The temporary type vector must not grow with arbitrary input.
        let mut excess = Vec::new();
        for _ in 0..33 {
            excess.extend_from_slice(&[0xe0, 0, 0, 1]);
        }
        assert!(!ffi::neteq_stock_red_payload_types(&excess, &mut types));
        assert!(types.is_empty());
    }

    #[test]
    fn prepared_decoder_handoff_preserves_identity_and_nullable_requests() {
        let environment = crate::Environment::builder().build().unwrap();
        for factory in [
            crate::AudioDecoderFactory::builtin().unwrap(),
            crate::AudioDecoderFactory::builtin_opus().unwrap(),
        ] {
            let before = factory
                .decoder_statistics()
                .map(|value| value.decoder_creations);
            assert!(ffi::neteq_decoder_handoff_contract(
                factory.native(),
                environment.native()
            ));
            if let Some(before) = before {
                // Seven explicit preparations, no implicit construction from
                // unmatched, repeated, cancelled or expired factory requests.
                assert_eq!(
                    factory.decoder_statistics().unwrap().decoder_creations - before,
                    7
                );
            }
        }
    }

    #[test]
    fn decoder_format_preserves_binary_sdp_and_signed_native_fields() {
        let mut sdp_format = SdpAudioFormat::new(vec![0xff, 0, b'x'], i32::MIN, usize::MAX);
        sdp_format.parameters.insert(vec![0xff, 0], vec![0xfe, 0]);
        let value = NetEqDecoderFormat {
            payload_type: i32::MIN,
            sample_rate_hz: i32::MAX,
            num_channels: i32::MIN,
            sdp_format,
        };
        assert_eq!(NetEqDecoderFormat::from_native(value.to_native()), value);
        assert_eq!(
            NetEqDecoderFormat::from_native(ffi::neteq_decoder_format_round_trip(
                &value.to_native()
            )),
            value
        );
    }
}
