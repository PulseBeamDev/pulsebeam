use crate::{RtcEvent, ffi};

/// Concrete traits in the pinned `rtp_header_extensions.h`. These are trait
/// identifiers, not wire IDs. Reads use the original native packet's mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum RtcEventRtpExtensionKind {
    AbsoluteSendTime,
    AbsoluteCaptureTime,
    AudioLevel,
    CsrcAudioLevels,
    TransmissionOffset,
    TransportSequenceNumber,
    TransportSequenceNumberV2,
    VideoOrientation,
    PlayoutDelay,
    VideoContentType,
    VideoTiming,
    ColorSpace,
    RtpStreamId,
    RepairedRtpStreamId,
    Mid,
    InbandComfortNoise,
    VideoFrameTrackingId,
    GenericFrameDescriptor,
    VideoLayersAllocation,
    CorruptionDetection,
    /// Stateless mandatory part only. Full decoding needs a dependency structure.
    DependencyDescriptorMandatory,
}

/// Presence and native parse success are separate. A present malformed
/// extension retains its raw bytes and has `decoded: None`.
#[derive(Clone, Debug, PartialEq)]
pub struct RtcEventRtpExtensionSnapshot {
    pub kind: RtcEventRtpExtensionKind,
    pub raw: Vec<u8>,
    pub decoded: Option<RtcEventDecodedRtpExtension>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum RtcEventDecodedRtpExtension {
    AbsoluteSendTime(u32),
    AbsoluteCaptureTime(RtcEventAbsoluteCaptureTime),
    AudioLevel(RtcEventAudioLevel),
    CsrcAudioLevels(Vec<u8>),
    TransmissionOffset(i32),
    TransportSequenceNumber(u16),
    TransportSequenceNumberV2 {
        sequence_number: u16,
        feedback_request: Option<RtcEventFeedbackRequest>,
    },
    VideoOrientation {
        rotation_degrees: u16,
    },
    PlayoutDelay {
        minimum_ms: i64,
        maximum_ms: i64,
    },
    VideoContentType(u8),
    VideoTiming(RtcEventVideoTiming),
    ColorSpace(RtcEventColorSpace),
    /// Native string bytes, without assuming the packet contains valid UTF-8.
    RtpStreamId(Vec<u8>),
    RepairedRtpStreamId(Vec<u8>),
    Mid(Vec<u8>),
    /// Parsed extension presence is independent of its optional level.
    InbandComfortNoise(Option<u8>),
    VideoFrameTrackingId(u16),
    GenericFrameDescriptor(RtcEventGenericFrameDescriptor),
    VideoLayersAllocation(RtcEventVideoLayersAllocation),
    CorruptionDetection(RtcEventCorruptionDetection),
    DependencyDescriptorMandatory(RtcEventDependencyDescriptorMandatory),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtcEventGenericFrameDescriptor {
    pub first_packet_in_subframe: bool,
    pub last_packet_in_subframe: bool,
    /// Native start fields are undefined on non-first packets and not read.
    pub start: Option<RtcEventGenericFrameStart>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtcEventGenericFrameStart {
    pub temporal_layer: i32,
    pub spatial_layer: i32,
    pub spatial_layers_bitmask: u8,
    pub width: i32,
    pub height: i32,
    pub frame_id: u16,
    pub frame_dependency_diffs: Vec<u16>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtcEventVideoLayersAllocation {
    pub rtp_stream_index: i32,
    pub resolution_and_frame_rate_valid: bool,
    pub active_spatial_layers: Vec<RtcEventVideoAllocationLayer>,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtcEventVideoAllocationLayer {
    pub rtp_stream_index: i32,
    pub spatial_id: i32,
    pub target_bitrate_per_temporal_layer: Vec<crate::RtcEventDataRate>,
    /// Native resolution/frame-rate values are valid only if the parent flag is set.
    pub resolution_and_frame_rate: Option<RtcEventVideoAllocationResolution>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RtcEventVideoAllocationResolution {
    pub width: u16,
    pub height: u16,
    pub frame_rate_fps: u8,
}
#[derive(Clone, Debug, PartialEq)]
pub struct RtcEventCorruptionDetection {
    pub sequence_index: i32,
    pub interpret_sequence_index_as_most_significant_bits: bool,
    pub standard_deviation: f64,
    pub luma_error_threshold: i32,
    pub chroma_error_threshold: i32,
    pub sample_values: Vec<f64>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RtcEventDependencyDescriptorMandatory {
    pub first_packet_in_frame: bool,
    pub last_packet_in_frame: bool,
    pub frame_number: i32,
    pub template_id: i32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RtcEventAbsoluteCaptureTime {
    /// Native NTP unsigned Q32.32 representation.
    pub absolute_capture_timestamp: u64,
    /// Native signed Q32.32 offset, not a rounded duration.
    pub estimated_capture_clock_offset: Option<i64>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RtcEventAudioLevel {
    pub voice_activity: bool,
    pub level: i32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RtcEventFeedbackRequest {
    pub include_timestamps: bool,
    pub sequence_count: i32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RtcEventVideoTiming {
    pub flags: u8,
    pub encode_start_delta_ms: u16,
    pub encode_finish_delta_ms: u16,
    pub packetization_finish_delta_ms: u16,
    pub pacer_exit_delta_ms: u16,
    pub network_timestamp_delta_ms: u16,
    pub network2_timestamp_delta_ms: u16,
}
/// Native color identifiers are preserved as their H.273/wire code values.
/// No display color conversion or HDR normalization is imposed.
#[derive(Clone, Debug, PartialEq)]
pub struct RtcEventColorSpace {
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
    pub range: u8,
    pub chroma_siting_horizontal: u8,
    pub chroma_siting_vertical: u8,
    pub hdr_metadata: Option<RtcEventHdrMetadata>,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RtcEventChromaticity {
    pub x: f32,
    pub y: f32,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RtcEventHdrMetadata {
    pub primary_r: RtcEventChromaticity,
    pub primary_g: RtcEventChromaticity,
    pub primary_b: RtcEventChromaticity,
    pub white_point: RtcEventChromaticity,
    pub luminance_max: f32,
    pub luminance_min: f32,
    pub max_content_light_level: i32,
    pub max_frame_average_light_level: i32,
}

impl RtcEvent {
    /// Read a concrete extension trait on an incoming or outgoing RTP event.
    /// Returns `None` for other event kinds, absent extensions, or traits not
    /// registered in the retained mapping. Parse failure is a present snapshot.
    pub fn rtp_extension(
        &self,
        kind: RtcEventRtpExtensionKind,
    ) -> Option<RtcEventRtpExtensionSnapshot> {
        let value = ffi::rtc_event_rtp_extension(self.native(), kind as u8);
        if !value.present {
            return None;
        }
        let decoded = if value.parsed {
            use RtcEventDecodedRtpExtension as D;
            use RtcEventRtpExtensionKind as K;
            Some(match kind {
                K::AbsoluteSendTime => D::AbsoluteSendTime(value.unsigned_value),
                K::AbsoluteCaptureTime => D::AbsoluteCaptureTime(RtcEventAbsoluteCaptureTime {
                    absolute_capture_timestamp: value.capture_timestamp,
                    estimated_capture_clock_offset: value
                        .has_optional
                        .then_some(value.capture_offset),
                }),
                K::AudioLevel => D::AudioLevel(RtcEventAudioLevel {
                    voice_activity: value.flag,
                    level: value.signed_value,
                }),
                K::CsrcAudioLevels => D::CsrcAudioLevels(value.bytes),
                K::TransmissionOffset => D::TransmissionOffset(value.signed_value),
                K::TransportSequenceNumber => D::TransportSequenceNumber(value.sequence),
                K::TransportSequenceNumberV2 => D::TransportSequenceNumberV2 {
                    sequence_number: value.sequence,
                    feedback_request: value.has_optional.then_some(RtcEventFeedbackRequest {
                        include_timestamps: value.feedback_include_timestamps,
                        sequence_count: value.feedback_sequence_count,
                    }),
                },
                K::VideoOrientation => D::VideoOrientation {
                    rotation_degrees: value.sequence,
                },
                K::PlayoutDelay => D::PlayoutDelay {
                    minimum_ms: value.min_delay_ms,
                    maximum_ms: value.max_delay_ms,
                },
                K::VideoContentType => D::VideoContentType(value.unsigned_value as u8),
                K::VideoTiming => D::VideoTiming(RtcEventVideoTiming {
                    flags: value.timing_flags,
                    encode_start_delta_ms: value.encode_start,
                    encode_finish_delta_ms: value.encode_finish,
                    packetization_finish_delta_ms: value.packetization_finish,
                    pacer_exit_delta_ms: value.pacer_exit,
                    network_timestamp_delta_ms: value.network_timestamp,
                    network2_timestamp_delta_ms: value.network2_timestamp,
                }),
                K::ColorSpace => D::ColorSpace(RtcEventColorSpace {
                    primaries: value.primary,
                    transfer: value.transfer,
                    matrix: value.matrix,
                    range: value.range,
                    chroma_siting_horizontal: value.chroma_horizontal,
                    chroma_siting_vertical: value.chroma_vertical,
                    hdr_metadata: value.has_hdr.then_some(RtcEventHdrMetadata {
                        primary_r: RtcEventChromaticity {
                            x: value.primary_r_x,
                            y: value.primary_r_y,
                        },
                        primary_g: RtcEventChromaticity {
                            x: value.primary_g_x,
                            y: value.primary_g_y,
                        },
                        primary_b: RtcEventChromaticity {
                            x: value.primary_b_x,
                            y: value.primary_b_y,
                        },
                        white_point: RtcEventChromaticity {
                            x: value.white_x,
                            y: value.white_y,
                        },
                        luminance_max: value.luminance_max,
                        luminance_min: value.luminance_min,
                        max_content_light_level: value.max_content_light_level,
                        max_frame_average_light_level: value.max_frame_average_light_level,
                    }),
                }),
                K::RtpStreamId => D::RtpStreamId(value.text),
                K::RepairedRtpStreamId => D::RepairedRtpStreamId(value.text),
                K::Mid => D::Mid(value.text),
                K::InbandComfortNoise => {
                    D::InbandComfortNoise(value.has_optional.then_some(value.unsigned_value as u8))
                }
                K::VideoFrameTrackingId => D::VideoFrameTrackingId(value.sequence),
                K::GenericFrameDescriptor => {
                    D::GenericFrameDescriptor(RtcEventGenericFrameDescriptor {
                        first_packet_in_subframe: value.first_packet,
                        last_packet_in_subframe: value.last_packet,
                        start: value.first_packet.then_some(RtcEventGenericFrameStart {
                            temporal_layer: value.temporal_id,
                            spatial_layer: value.spatial_id,
                            spatial_layers_bitmask: value.spatial_mask,
                            width: value.width,
                            height: value.height,
                            frame_id: value.frame_id,
                            frame_dependency_diffs: value.dependency_diffs,
                        }),
                    })
                }
                K::VideoLayersAllocation => {
                    D::VideoLayersAllocation(RtcEventVideoLayersAllocation {
                        rtp_stream_index: value.rtp_stream_index,
                        resolution_and_frame_rate_valid: value.has_resolution_and_frame_rate,
                        active_spatial_layers: value
                            .allocation_layers
                            .into_iter()
                            .map(|layer| RtcEventVideoAllocationLayer {
                                rtp_stream_index: layer.rtp_stream_index,
                                spatial_id: layer.spatial_id,
                                target_bitrate_per_temporal_layer: layer
                                    .rates
                                    .into_iter()
                                    .map(|rate| match rate.kind {
                                        -1 => crate::RtcEventDataRate::NegativeInfinity,
                                        1 => crate::RtcEventDataRate::PositiveInfinity,
                                        _ => crate::RtcEventDataRate::BitsPerSecond(rate.bps),
                                    })
                                    .collect(),
                                resolution_and_frame_rate: value
                                    .has_resolution_and_frame_rate
                                    .then_some(RtcEventVideoAllocationResolution {
                                        width: layer.width,
                                        height: layer.height,
                                        frame_rate_fps: layer.frame_rate_fps,
                                    }),
                            })
                            .collect(),
                    })
                }
                K::CorruptionDetection => D::CorruptionDetection(RtcEventCorruptionDetection {
                    sequence_index: value.sequence_index,
                    interpret_sequence_index_as_most_significant_bits: value
                        .interpret_sequence_index_as_msb,
                    standard_deviation: value.std_dev,
                    luma_error_threshold: value.luma_error_threshold,
                    chroma_error_threshold: value.chroma_error_threshold,
                    sample_values: value.sample_values,
                }),
                K::DependencyDescriptorMandatory => {
                    D::DependencyDescriptorMandatory(RtcEventDependencyDescriptorMandatory {
                        first_packet_in_frame: value.first_packet,
                        last_packet_in_frame: value.last_packet,
                        frame_number: value.frame_number,
                        template_id: value.template_id,
                    })
                }
            })
        } else {
            None
        };
        Some(RtcEventRtpExtensionSnapshot {
            kind,
            raw: value.raw,
            decoded,
        })
    }
}
