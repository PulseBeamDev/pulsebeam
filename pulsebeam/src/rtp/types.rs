use std::{fmt, num::NonZeroU32, ops::Deref, time::SystemTime};

use pulsebeam_core::dd::{DependencyDescriptor, RawDependencyDescriptor};
use tokio::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketProvenance {
    pub received_at: Instant,
    pub packet_id: u64,
    pub stream_id: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Frequency(NonZeroU32);

impl Frequency {
    pub const NINETY_KHZ: Self = Self(NonZeroU32::new(90_000).expect("non-zero clock rate"));
    pub const FORTY_EIGHT_KHZ: Self = Self(NonZeroU32::new(48_000).expect("non-zero clock rate"));
    pub const HUNDREDTHS: Self = Self(NonZeroU32::new(100).expect("non-zero clock rate"));

    pub const fn new(value: u32) -> Option<Self> {
        match NonZeroU32::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MediaTime {
    numer: u64,
    frequency: Frequency,
}

impl MediaTime {
    pub const fn new(numer: u64, frequency: Frequency) -> Self {
        Self { numer, frequency }
    }

    pub const fn numer(self) -> u64 {
        self.numer
    }

    pub const fn frequency(self) -> Frequency {
        self.frequency
    }

    pub const fn from_90khz(numer: u64) -> Self {
        Self::new(numer, Frequency::NINETY_KHZ)
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SequenceNumber(u64);

impl SequenceNumber {
    pub const fn as_u16(self) -> u16 {
        self.0 as u16
    }

    pub const fn wrapping_add(self, value: u64) -> Self {
        Self(self.0.wrapping_add(value))
    }

    pub const fn wrapping_sub(self, value: u64) -> Self {
        Self(self.0.wrapping_sub(value))
    }
}

impl Deref for SequenceNumber {
    type Target = u64;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<u64> for SequenceNumber {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl From<SequenceNumber> for u64 {
    fn from(value: SequenceNumber) -> Self {
        value.0
    }
}

impl fmt::Display for SequenceNumber {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ssrc(u32);

impl Ssrc {
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl Deref for Ssrc {
    type Target = u32;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<u32> for Ssrc {
    fn from(value: u32) -> Self {
        Self(value)
    }
}

impl fmt::Display for Ssrc {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PayloadType(u8);

impl PayloadType {
    pub fn new(value: u8) -> Option<Self> {
        (value < 128).then_some(Self(value))
    }

    pub const fn get(self) -> u8 {
        self.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MediaSectionId([u8; 16]);

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EncodingId([u8; 8]);

macro_rules! fixed_id {
    ($type:ident, $length:expr) => {
        impl From<&str> for $type {
            fn from(value: &str) -> Self {
                let mut bytes = [b' '; $length];
                for (destination, source) in bytes.iter_mut().zip(value.bytes()) {
                    *destination = if source.is_ascii_alphanumeric() {
                        source
                    } else {
                        b'_'
                    };
                }
                Self(bytes)
            }
        }

        impl Deref for $type {
            type Target = str;

            fn deref(&self) -> &Self::Target {
                let length = self
                    .0
                    .iter()
                    .position(|byte| *byte == b' ')
                    .unwrap_or($length);
                std::str::from_utf8(&self.0[..length]).expect("packet identifiers are ASCII")
            }
        }

        impl fmt::Display for $type {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self)
            }
        }

        impl fmt::Debug for $type {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_tuple(stringify!($type))
                    .field(&&**self)
                    .finish()
            }
        }
    };
}

fixed_id!(MediaSectionId, 16);
fixed_id!(EncodingId, 8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKind {
    Audio,
    Video,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenderReport {
    pub ssrc: Ssrc,
    pub ntp_time: SystemTime,
    pub rtp_time: MediaTime,
    pub sender_packet_count: u32,
    pub sender_octet_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PacketExtensions {
    pub mid: Option<MediaSectionId>,
    pub rid: Option<EncodingId>,
    pub audio_level: Option<i8>,
    pub play_delay_min: Option<MediaTime>,
    pub play_delay_max: Option<MediaTime>,
    pub raw_dependency_descriptor: Option<RawDependencyDescriptor>,
    pub dependency_descriptor: Option<DependencyDescriptor>,
    pub video_layers_allocation: Option<VideoLayersAllocation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoLayersAllocation {
    pub current_simulcast_stream_index: u8,
    pub simulcast_streams: Vec<SimulcastStreamAllocation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimulcastStreamAllocation {
    pub spatial_layers: Vec<SpatialLayerAllocation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpatialLayerAllocation {
    pub temporal_layers: Vec<TemporalLayerAllocation>,
    pub resolution_and_framerate: Option<ResolutionAndFramerate>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemporalLayerAllocation {
    pub cumulative_kbps: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionAndFramerate {
    pub width: u16,
    pub height: u16,
    pub framerate: u8,
}
