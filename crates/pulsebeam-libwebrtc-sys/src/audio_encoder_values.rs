//! Owning values for the pinned AudioEncoder and AudioEncoderFactory contracts.
//! Representability is separate from operation-specific native preconditions.
use crate::{AudioCodecPairId, DataRate, TimeDelta};

/// Exact native codec metric tag, including unknown future values.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub struct AudioEncoderCodecType(pub i32);
impl AudioEncoderCodecType {
    pub const OTHER: Self = Self(0);
    pub const OPUS: Self = Self(1);
    pub const ISAC: Self = Self(2);
    pub const PCMA: Self = Self(3);
    pub const PCMU: Self = Self(4);
    pub const G722: Self = Self(5);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioEncoderApplication {
    Speech,
    Audio,
}

/// Metadata for one contiguous encoding within an appended payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioEncodedInfoLeaf {
    pub encoded_bytes: usize,
    pub encoded_timestamp: u32,
    pub payload_type: i32,
    pub send_even_if_empty: bool,
    pub speech: bool,
    pub encoder_type: AudioEncoderCodecType,
}
impl Default for AudioEncodedInfoLeaf {
    fn default() -> Self {
        Self {
            encoded_bytes: 0,
            encoded_timestamp: 0,
            payload_type: 0,
            send_even_if_empty: false,
            speech: true,
            encoder_type: AudioEncoderCodecType::OTHER,
        }
    }
}

impl AudioEncodedInfoLeaf {
    pub(crate) fn from_native(value: crate::ffi::FfiAudioEncodedInfoLeaf) -> Self {
        Self {
            encoded_bytes: value.encoded_bytes,
            encoded_timestamp: value.encoded_timestamp,
            payload_type: value.payload_type,
            send_even_if_empty: value.send_even_if_empty,
            speech: value.speech,
            encoder_type: AudioEncoderCodecType(value.encoder_type),
        }
    }
}

/// Complete native packet metadata. Redundant leaves are in payload order.
/// If nonempty, their encoded byte counts must sum to the primary byte count
/// at encoding admission. Building a value alone does not run that admission.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AudioEncodedInfo {
    pub primary: AudioEncodedInfoLeaf,
    pub redundant: Vec<AudioEncodedInfoLeaf>,
}

/// Only the bytes newly appended by one encoding operation, not any preexisting
/// prefix in the native destination buffer. Zero bytes is a valid result.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AudioEncoderOutput {
    pub bytes: Vec<u8>,
    pub info: AudioEncodedInfo,
}

/// Native Create options. Pair IDs may link multiple encoders and decoders;
/// callers and providers must not assume a one-to-one pairing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioEncoderOptions {
    pub payload_type: i32,
    pub codec_pair_id: Option<AudioCodecPairId>,
}
impl Default for AudioEncoderOptions {
    fn default() -> Self {
        Self {
            payload_type: -1,
            codec_pair_id: None,
        }
    }
}

/// All optional Audio Network Adaptation statistics. Absence means the
/// corresponding controller is disabled or has no reported statistic, not zero.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AudioNetworkAdaptationStatistics {
    pub bitrate_action_counter: Option<u32>,
    pub channel_action_counter: Option<u32>,
    pub dtx_action_counter: Option<u32>,
    pub fec_action_counter: Option<u32>,
    pub frame_length_increase_counter: Option<u32>,
    pub frame_length_decrease_counter: Option<u32>,
    pub uplink_packet_loss_fraction: Option<f32>,
}

/// Ordered by the operation that consumes it; values themselves preserve native
/// TimeDelta domains, including native infinite values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioEncoderFrameLengthRange {
    pub minimum: TimeDelta,
    pub maximum: TimeDelta,
}

/// Complete pinned bitrate-allocation update. Unused fields still retain
/// their native state; consumer preconditions are checked only at dispatch.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BitrateAllocationUpdate {
    pub target_bitrate: DataRate,
    pub packet_loss_ratio: f64,
    pub round_trip_time: TimeDelta,
    pub bwe_period: TimeDelta,
    pub cwnd_reduce_ratio: f64,
}
impl Default for BitrateAllocationUpdate {
    fn default() -> Self {
        Self {
            target_bitrate: DataRate::BitsPerSecond(0),
            packet_loss_ratio: 0.0,
            round_trip_time: TimeDelta::plus_infinity(),
            bwe_period: TimeDelta::plus_infinity(),
            cwnd_reduce_ratio: 0.0,
        }
    }
}
impl BitrateAllocationUpdate {
    pub(crate) fn to_native(self) -> crate::ffi::FfiBitrateAllocationUpdate {
        let (target_bitrate, target_kind) = match self.target_bitrate {
            DataRate::BitsPerSecond(value) => (value, 0),
            DataRate::PositiveInfinity => (0, 1),
            DataRate::NegativeInfinity => (0, -1),
        };
        crate::ffi::FfiBitrateAllocationUpdate {
            target_bitrate,
            target_kind,
            packet_loss_ratio: self.packet_loss_ratio,
            round_trip_time: self.round_trip_time.native_microsecond_bits(),
            bwe_period: self.bwe_period.native_microsecond_bits(),
            cwnd_reduce_ratio: self.cwnd_reduce_ratio,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AudioEncoderBitrateRange {
    pub minimum: DataRate,
    pub maximum: DataRate,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_preserve_native_absence_and_packet_metadata() {
        let leaf = AudioEncodedInfoLeaf::default();
        assert!(leaf.speech);
        assert!(!leaf.send_even_if_empty);
        assert_eq!(leaf.payload_type, 0);
        assert_eq!(AudioEncoderOptions::default().payload_type, -1);
        assert!(AudioEncoderOptions::default().codec_pair_id.is_none());
        assert!(
            AudioNetworkAdaptationStatistics::default()
                .uplink_packet_loss_fraction
                .is_none()
        );
        assert_eq!(AudioEncoderOutput::default().info.primary, leaf);
    }
    #[test]
    fn options_retain_native_pair_identity() {
        let id = AudioCodecPairId::create();
        let options = AudioEncoderOptions {
            payload_type: 111,
            codec_pair_id: Some(id.clone()),
        };
        assert_eq!(options.clone().codec_pair_id.unwrap(), id);
        let copied = id.copy_value();
        assert_eq!(copied, id);
        let numeric = copied.numeric_representation();
        drop(options);
        drop(id);
        std::thread::spawn(move || assert_eq!(copied.numeric_representation(), numeric))
            .join()
            .unwrap();
    }
}
