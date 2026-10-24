//! Owned audio/video receive-packet provenance, distinct from the network
//! controller's `RtpPacketInfo` message.
use std::{ops::Deref, sync::Arc};

use crate::{TimeDelta, Timestamp, ffi};

/// RTP absolute capture time in UQ32.32 NTP bits, not integer microseconds.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AbsoluteCaptureTime {
    pub absolute_capture_timestamp: u64,
    /// Signed Q32.32 offset between remote sender and capture clocks.
    pub estimated_capture_clock_offset: Option<i64>,
}
impl AbsoluteCaptureTime {
    pub(crate) fn to_native(self) -> ffi::FfiAbsoluteCaptureTime {
        ffi::FfiAbsoluteCaptureTime {
            absolute_capture_timestamp: self.absolute_capture_timestamp,
            has_estimated_capture_clock_offset: self.estimated_capture_clock_offset.is_some(),
            estimated_capture_clock_offset: self.estimated_capture_clock_offset.unwrap_or(0),
        }
    }
    pub(crate) fn from_native(value: ffi::FfiAbsoluteCaptureTime) -> Self {
        Self {
            absolute_capture_timestamp: value.absolute_capture_timestamp,
            estimated_capture_clock_offset: value
                .has_estimated_capture_clock_offset
                .then_some(value.estimated_capture_clock_offset),
        }
    }
}

/// Complete owning `webrtc::RtpPacketInfo` value. Its optional audio level is
/// a native uint8, unlike RTP header `AudioLevel`'s checked 0..=127 value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtpReceivedPacketInfo {
    pub sequence_number: u16,
    pub ssrc: u32,
    pub csrcs: Vec<u32>,
    pub rtp_timestamp: u32,
    pub receive_time: Timestamp,
    pub audio_level: Option<u8>,
    pub absolute_capture_time: Option<AbsoluteCaptureTime>,
    /// Offset between local and capture clocks, not remote sender/capture clocks.
    pub local_capture_clock_offset: Option<TimeDelta>,
}
impl Default for RtpReceivedPacketInfo {
    fn default() -> Self {
        Self {
            sequence_number: 0,
            ssrc: 0,
            csrcs: Vec::new(),
            rtp_timestamp: 0,
            receive_time: Timestamp::minus_infinity(),
            audio_level: None,
            absolute_capture_time: None,
            local_capture_clock_offset: None,
        }
    }
}
impl RtpReceivedPacketInfo {
    /// Native compatibility constructor semantics; sequence starts at zero.
    pub fn new(ssrc: u32, csrcs: Vec<u32>, rtp_timestamp: u32, receive_time: Timestamp) -> Self {
        Self {
            ssrc,
            csrcs,
            rtp_timestamp,
            receive_time,
            ..Self::default()
        }
    }
    /// Calls the native compatibility constructor, including its CSRC-count
    /// clamp and capture/audio-level extraction. No RTP wire encoding occurs.
    pub fn from_header(
        header: &crate::RtpHeaderValues,
        receive_time: Timestamp,
    ) -> Result<Self, crate::RtpValueError> {
        let mut valid = false;
        let value = ffi::media_packet_info_from_header(
            &header.to_native(),
            receive_time.native_microsecond_bits(),
            &mut valid,
        );
        if !valid {
            return Err(crate::RtpValueError::InvalidRotation);
        }
        Ok(Self::from_native(value))
    }
    pub(crate) fn to_native(&self) -> ffi::FfiRtpReceivedPacketInfo {
        ffi::FfiRtpReceivedPacketInfo {
            sequence_number: self.sequence_number,
            ssrc: self.ssrc,
            csrcs: self.csrcs.clone(),
            rtp_timestamp: self.rtp_timestamp,
            receive_time_us: self.receive_time.native_microsecond_bits(),
            has_audio_level: self.audio_level.is_some(),
            audio_level: self.audio_level.unwrap_or(0),
            has_absolute_capture_time: self.absolute_capture_time.is_some(),
            absolute_capture_time: self
                .absolute_capture_time
                .unwrap_or(AbsoluteCaptureTime {
                    absolute_capture_timestamp: 0,
                    estimated_capture_clock_offset: None,
                })
                .to_native(),
            has_local_capture_clock_offset: self.local_capture_clock_offset.is_some(),
            local_capture_clock_offset_us: self
                .local_capture_clock_offset
                .map_or(0, TimeDelta::native_microsecond_bits),
        }
    }
    pub(crate) fn from_native(value: ffi::FfiRtpReceivedPacketInfo) -> Self {
        Self {
            sequence_number: value.sequence_number,
            ssrc: value.ssrc,
            csrcs: value.csrcs,
            rtp_timestamp: value.rtp_timestamp,
            receive_time: Timestamp::from_native_microsecond_bits(value.receive_time_us),
            audio_level: value.has_audio_level.then_some(value.audio_level),
            absolute_capture_time: value
                .has_absolute_capture_time
                .then(|| AbsoluteCaptureTime::from_native(value.absolute_capture_time)),
            local_capture_clock_offset: value.has_local_capture_clock_offset.then(|| {
                TimeDelta::from_native_microsecond_bits(value.local_capture_clock_offset_us)
            }),
        }
    }
}

/// Cheap-copy, immutable owning packet-info collection with safe slice access.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RtpReceivedPacketInfos(Arc<[RtpReceivedPacketInfo]>);
impl From<Vec<RtpReceivedPacketInfo>> for RtpReceivedPacketInfos {
    fn from(entries: Vec<RtpReceivedPacketInfo>) -> Self {
        Self(entries.into())
    }
}
impl Deref for RtpReceivedPacketInfos {
    type Target = [RtpReceivedPacketInfo];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl RtpReceivedPacketInfos {
    pub(crate) fn to_native(&self) -> Vec<ffi::FfiRtpReceivedPacketInfo> {
        self.iter().map(RtpReceivedPacketInfo::to_native).collect()
    }
    pub(crate) fn from_native(entries: Vec<ffi::FfiRtpReceivedPacketInfo>) -> Self {
        entries
            .into_iter()
            .map(RtpReceivedPacketInfo::from_native)
            .collect::<Vec<_>>()
            .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ffi_round_trip_preserves_all_optionals_widths_and_infinities() {
        for receive_time in [
            Timestamp::minus_infinity(),
            Timestamp::plus_infinity(),
            Timestamp::from_microseconds(-1).unwrap(),
        ] {
            for local_capture_clock_offset in [
                None,
                Some(TimeDelta::zero()),
                Some(TimeDelta::minus_infinity()),
                Some(TimeDelta::plus_infinity()),
            ] {
                let packet = RtpReceivedPacketInfo {
                    sequence_number: u16::MAX,
                    ssrc: u32::MAX,
                    csrcs: vec![0, u32::MAX, 1],
                    rtp_timestamp: u32::MAX,
                    receive_time,
                    audio_level: Some(u8::MAX),
                    absolute_capture_time: Some(AbsoluteCaptureTime {
                        absolute_capture_timestamp: u64::MAX,
                        estimated_capture_clock_offset: Some(i64::MIN),
                    }),
                    local_capture_clock_offset,
                };
                assert_eq!(
                    RtpReceivedPacketInfo::from_native(ffi::media_packet_info_round_trip(
                        &packet.to_native()
                    )),
                    packet
                );
                let entries: RtpReceivedPacketInfos =
                    vec![packet, RtpReceivedPacketInfo::default()].into();
                assert_eq!(
                    RtpReceivedPacketInfos::from_native(ffi::media_packet_infos_round_trip(
                        &entries.to_native()
                    )),
                    entries
                );
                assert_eq!(entries.clone().as_ptr(), entries.as_ptr());
            }
        }
        let empty = RtpReceivedPacketInfo::default();
        assert_eq!(
            RtpReceivedPacketInfo::from_native(ffi::media_packet_info_round_trip(
                &empty.to_native()
            )),
            empty
        );
        let present_zero = AbsoluteCaptureTime {
            absolute_capture_timestamp: 0,
            estimated_capture_clock_offset: Some(0),
        };
        assert_eq!(
            AbsoluteCaptureTime::from_native(present_zero.to_native()),
            present_zero
        );
    }
}
