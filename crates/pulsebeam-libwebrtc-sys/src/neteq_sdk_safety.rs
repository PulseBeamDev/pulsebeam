//! Admission for the pinned SDK peer receive/mixer consumers, not standalone NetEq.
//! VoIP AudioIngress is not covered: its zero RTP clock has no fallback.
//! These checks must run before SDK consumption. They do not normalize values,
//! sample clocks, query providers or substitute a product. Factory wiring and
//! delay/statistics reconciliation remain unfinished.
use crate::{AudioFrameFields, AudioFrameSpeechType, NetEqDecoderFormat};

// PushResampler -> SincResampler: request_frames - kernel_size / 2 must
// exceed kernel_size (32). The SDK's 10-ms upper limit is 384000 / 100.
const MIN_RESAMPLER_SOURCE_FRAMES: usize = 49;
const MAX_RESAMPLER_SOURCE_FRAMES: usize = 3840;
const MAX_RESAMPLER_CHANNELS: usize = 8;

pub(crate) fn audio_fields_are_safe(fields: &AudioFrameFields) -> bool {
    // Initialization and fixed backing capacity are separate AudioFrameMut
    // obligations, checked by the callback bridge. Do not infer them here.
    fields.sample_rate_hz >= 100
        && fields.samples_per_channel == fields.sample_rate_hz as usize / 100
        && (MIN_RESAMPLER_SOURCE_FRAMES..=MAX_RESAMPLER_SOURCE_FRAMES).contains(&fields.samples_per_channel)
        && (1..=MAX_RESAMPLER_CHANNELS).contains(&fields.num_channels)
        // CallStatistics DCHECKs on undefined, even if the frame is muted.
        // Unnamed representable enum tags are not table indices in this caller.
        && fields.speech_type != AudioFrameSpeechType::UNDEFINED
}

pub(crate) fn last_output_rate_is_safe(rate: i32) -> bool {
    // DefaultOutputRateCalculator DCHECKs this range and dereferences the
    // lower_bound result without an end check in release. Non-native rates
    // inside the interval remain valid and are rounded up by the SDK itself.
    (8000..=48000).contains(&rate)
}

pub(crate) fn decoder_format_is_safe(format: &NetEqDecoderFormat) -> bool {
    // PreferredSampleRate takes max(decoder.sample_rate_hz, last_output_rate).
    // A negative decoder sample rate cannot raise that max; do not manufacture
    // a lower bound on a field this consumer does not divide by or index with.
    format.sample_rate_hz <= 48000 && rtp_clock_rate_is_safe(format.sdp_format.clockrate_hz)
}

fn rtp_clock_rate_is_safe(rate: i32) -> bool {
    // Zero selects the already-guarded last-output rate in ChannelReceive.
    // For nonzero rates, elapsed time divides by rate / 1000. The playout
    // timestamp additionally computes uint16 device_delay * (rate / 1000)
    // after promotion to signed int. Prove safety for every uint16 delay.
    rate == 0 || (rate >= 1000 && rate / 1000 <= i32::MAX / u16::MAX as i32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SdpAudioFormat;

    fn fields(rate: i32, channels: usize) -> AudioFrameFields {
        AudioFrameFields {
            sample_rate_hz: rate,
            samples_per_channel: rate.max(0) as usize / 100,
            num_channels: channels,
            speech_type: AudioFrameSpeechType::NORMAL_SPEECH,
            ..Default::default()
        }
    }
    #[test]
    fn resampler_limits_include_the_kernel_lower_bound_and_multichannel_domain() {
        for rate in [4900, 4999, 8000, 44100, 384000, 384099] {
            assert!(audio_fields_are_safe(&fields(rate, 1)));
        }
        assert!(audio_fields_are_safe(&fields(44100, 8)));
        for rate in [i32::MIN, 0, 100, 4800, 4899, 384100, i32::MAX] {
            assert!(!audio_fields_are_safe(&fields(rate, 1)));
        }
        for channels in [0, 9, 16, usize::MAX] {
            assert!(!audio_fields_are_safe(&fields(8000, channels)));
        }
        let mut frame = fields(8000, 1);
        frame.samples_per_channel += 1;
        assert!(!audio_fields_are_safe(&frame));
        frame = fields(8000, 1);
        frame.speech_type = AudioFrameSpeechType::UNDEFINED;
        assert!(!audio_fields_are_safe(&frame));
    }
    #[test]
    fn mixer_preference_does_not_require_a_native_rate_whitelist() {
        for rate in [8000, 8001, 16000, 32000, 44100, 48000] {
            assert!(last_output_rate_is_safe(rate));
        }
        for rate in [i32::MIN, 0, 7999, 48001, 192000, i32::MAX] {
            assert!(!last_output_rate_is_safe(rate));
        }
    }
    #[test]
    fn rtp_rates_prove_division_and_promoted_device_delay_multiplication() {
        let maximum_quotient = i32::MAX / u16::MAX as i32;
        let maximum = maximum_quotient * 1000 + 999;
        for rate in [0, 1000, 44100, 48000, maximum] {
            assert!(rtp_clock_rate_is_safe(rate));
        }
        for rate in [i32::MIN, -1000, -1, 1, 999, maximum + 1, i32::MAX] {
            assert!(!rtp_clock_rate_is_safe(rate));
        }
        for sample_rate in [i32::MIN, 0, 44100, 48000] {
            let format = NetEqDecoderFormat {
                payload_type: i32::MAX,
                num_channels: i32::MIN,
                sample_rate_hz: sample_rate,
                sdp_format: SdpAudioFormat::new(b"custom".to_vec(), maximum, usize::MAX),
            };
            assert!(decoder_format_is_safe(&format));
        }
        let format = NetEqDecoderFormat {
            payload_type: 0,
            num_channels: 1,
            sample_rate_hz: 48001,
            sdp_format: SdpAudioFormat::new(b"custom".to_vec(), 48000, 1),
        };
        assert!(!decoder_format_is_safe(&format));
    }
}
