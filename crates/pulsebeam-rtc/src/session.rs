#![allow(
    clippy::disallowed_types,
    reason = "the v3 public value contract uses immutable Arc-backed strings, slices, and Bytes"
)]

use std::{fmt, net::SocketAddr, num::NonZeroU16, sync::Arc, time::Duration};

use crate::{AcceptError, DataChannelId, SenderId};

const MAX_PLAYOUT_TICKS: u16 = 4095;
const MILLIS_PER_PLAYOUT_TICK: u64 = 10;
const MAX_LOCAL_CANDIDATES: usize = 16;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SessionCapabilities {
    _private: (),
}

impl SessionCapabilities {
    pub const fn new() -> Self {
        Self { _private: () }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct SdpOffer(Arc<str>);

impl SdpOffer {
    pub fn new(value: impl Into<Arc<str>>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SdpOffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SdpOffer")
            .field("bytes", &self.0.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct SdpAnswer(Arc<str>);

impl SdpAnswer {
    #[allow(
        dead_code,
        reason = "answers are constructed by Connection::accept in Plan 02"
    )]
    pub(crate) fn new(value: impl Into<Arc<str>>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SdpAnswer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SdpAnswer")
            .field("bytes", &self.0.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaKind {
    Audio,
    Video,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketFeedbackKind {
    TransportWide,
    Rfc8888,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionInfo {
    pub feedback: Option<PacketFeedbackKind>,
    pub senders: Arc<[SenderInfo]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SenderInfo {
    pub id: SenderId,
    pub kind: MediaKind,
    pub mid: Arc<str>,
    pub rtp_clock_rate: u32,
    pub supports_rtx: bool,
    pub signals_playout_delay: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum UnknownExtensionPolicy {
    #[default]
    Drop,
    Opaque,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalCandidate {
    Udp(SocketAddr),
    TcpPassive(SocketAddr),
}

impl LocalCandidate {
    pub(crate) const fn address(self) -> SocketAddr {
        match self {
            Self::Udp(address) | Self::TcpPassive(address) => address,
        }
    }

    const fn is_valid(self) -> bool {
        let address = self.address();
        if address.port() == 0 {
            return false;
        }
        match address {
            SocketAddr::V4(address) => {
                let ip = address.ip();
                !ip.is_unspecified() && !ip.is_multicast() && !ip.is_broadcast()
            }
            SocketAddr::V6(address) => {
                let ip = address.ip();
                !ip.is_unspecified()
                    && !ip.is_multicast()
                    && address.flowinfo() == 0
                    && address.scope_id() == 0
            }
        }
    }

    fn same_transport_and_address(self, other: Self) -> bool {
        matches!(
            (self, other),
            (Self::Udp(left), Self::Udp(right))
                | (Self::TcpPassive(left), Self::TcpPassive(right)) if left == right
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectionLimits {
    pub max_unsignaled_encodings: u16,
    pub max_data_channels: u16,
    pub max_inbound_data_message_bytes: usize,
    pub max_buffered_data_bytes: usize,
    pub max_queued_media_bytes: usize,
    pub max_retransmission_bytes: usize,
}

impl ConnectionLimits {
    pub const DEFAULT_MAX_UNSIGNALED_ENCODINGS: u16 = 32;
    pub const HARD_MAX_UNSIGNALED_ENCODINGS: u16 = 1024;
    pub const DEFAULT_MAX_DATA_CHANNELS: u16 = 256;
    pub const HARD_MAX_DATA_CHANNELS: u16 = 4096;
    pub const DEFAULT_MAX_INBOUND_DATA_MESSAGE_BYTES: usize = 1024 * 1024;
    pub const HARD_MAX_INBOUND_DATA_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
    pub const DEFAULT_MAX_BUFFERED_DATA_BYTES: usize = 8 * 1024 * 1024;
    pub const HARD_MAX_BUFFERED_DATA_BYTES: usize = 64 * 1024 * 1024;
    pub const DEFAULT_MAX_QUEUED_MEDIA_BYTES: usize = 8 * 1024 * 1024;
    pub const HARD_MAX_QUEUED_MEDIA_BYTES: usize = 64 * 1024 * 1024;
    pub const DEFAULT_MAX_RETRANSMISSION_BYTES: usize = 16 * 1024 * 1024;
    pub const HARD_MAX_RETRANSMISSION_BYTES: usize = 128 * 1024 * 1024;

    pub const fn validate(self) -> Result<Self, AcceptError> {
        if self.max_unsignaled_encodings > Self::HARD_MAX_UNSIGNALED_ENCODINGS
            || self.max_data_channels > Self::HARD_MAX_DATA_CHANNELS
            || self.max_inbound_data_message_bytes > Self::HARD_MAX_INBOUND_DATA_MESSAGE_BYTES
            || self.max_buffered_data_bytes > Self::HARD_MAX_BUFFERED_DATA_BYTES
            || self.max_queued_media_bytes > Self::HARD_MAX_QUEUED_MEDIA_BYTES
            || self.max_retransmission_bytes > Self::HARD_MAX_RETRANSMISSION_BYTES
        {
            return Err(AcceptError::InvalidConfiguration);
        }
        Ok(self)
    }
}

impl Default for ConnectionLimits {
    fn default() -> Self {
        Self {
            max_unsignaled_encodings: Self::DEFAULT_MAX_UNSIGNALED_ENCODINGS,
            max_data_channels: Self::DEFAULT_MAX_DATA_CHANNELS,
            max_inbound_data_message_bytes: Self::DEFAULT_MAX_INBOUND_DATA_MESSAGE_BYTES,
            max_buffered_data_bytes: Self::DEFAULT_MAX_BUFFERED_DATA_BYTES,
            max_queued_media_bytes: Self::DEFAULT_MAX_QUEUED_MEDIA_BYTES,
            max_retransmission_bytes: Self::DEFAULT_MAX_RETRANSMISSION_BYTES,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConnectionConfig {
    pub capabilities: SessionCapabilities,
    pub local_candidates: Vec<LocalCandidate>,
    pub limits: ConnectionLimits,
    pub default_audio_policy: SenderPolicy,
    pub default_video_policy: SenderPolicy,
    pub unknown_extension_policy: UnknownExtensionPolicy,
}

impl ConnectionConfig {
    pub fn validate(self) -> Result<Self, AcceptError> {
        self.limits.validate()?;
        if self.local_candidates.len() > MAX_LOCAL_CANDIDATES {
            return Err(AcceptError::SessionLimitExceeded);
        }
        for candidate in self.local_candidates.iter().copied() {
            if !candidate.is_valid() {
                return Err(AcceptError::InvalidConfiguration);
            }
        }
        for (index, candidate) in self.local_candidates.iter().copied().enumerate() {
            if self
                .local_candidates
                .iter()
                .copied()
                .skip(index.saturating_add(1))
                .any(|other| candidate.same_transport_and_address(other))
            {
                return Err(AcceptError::InvalidConfiguration);
            }
        }
        Ok(self)
    }
}

impl Default for ConnectionConfig {
    fn default() -> Self {
        Self {
            capabilities: SessionCapabilities::default(),
            local_candidates: Vec::new(),
            limits: ConnectionLimits::default(),
            default_audio_policy: SenderPolicy {
                playout_delay: PlayoutDelay::ZERO,
                priority: MediaPriority::HIGH,
                desired_bitrate: MediaPayloadBitrate::from_bps(0),
            },
            default_video_policy: SenderPolicy {
                playout_delay: PlayoutDelay::ZERO,
                priority: MediaPriority::MEDIUM,
                desired_bitrate: MediaPayloadBitrate::from_bps(0),
            },
            unknown_extension_policy: UnknownExtensionPolicy::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SenderPolicy {
    pub playout_delay: PlayoutDelay,
    pub priority: MediaPriority,
    pub desired_bitrate: MediaPayloadBitrate,
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct MediaPayloadBitrate(u64);

impl MediaPayloadBitrate {
    pub const fn from_bps(value: u64) -> Self {
        Self(value)
    }

    pub const fn as_bps(self) -> u64 {
        self.0
    }
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct MediaPriority(NonZeroU16);

impl MediaPriority {
    pub const VERY_LOW: Self = Self::new_const(1);
    pub const LOW: Self = Self::new_const(2);
    pub const MEDIUM: Self = Self::new_const(4);
    pub const HIGH: Self = Self::new_const(8);

    #[allow(
        clippy::unreachable,
        reason = "all callers pass statically known nonzero priority constants"
    )]
    const fn new_const(weight: u16) -> Self {
        match NonZeroU16::new(weight) {
            Some(weight) => Self(weight),
            None => unreachable!(),
        }
    }

    pub fn new(weight: u16) -> Result<Self, PolicyError> {
        if weight > 256 {
            return Err(PolicyError::PriorityOutOfRange);
        }
        NonZeroU16::new(weight)
            .map(Self)
            .ok_or(PolicyError::PriorityOutOfRange)
    }

    pub const fn weight(self) -> u16 {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlayoutDelay {
    min_ticks: u16,
    max_ticks: u16,
}

impl PlayoutDelay {
    pub const ZERO: Self = Self {
        min_ticks: 0,
        max_ticks: 0,
    };

    pub const fn from_ticks(min: u16, max: u16) -> Result<Self, PolicyError> {
        if min > max || max > MAX_PLAYOUT_TICKS {
            return Err(PolicyError::PlayoutRange);
        }
        Ok(Self {
            min_ticks: min,
            max_ticks: max,
        })
    }

    pub fn from_millis_exact(min: u64, max: u64) -> Result<Self, PolicyError> {
        if !min.is_multiple_of(MILLIS_PER_PLAYOUT_TICK)
            || !max.is_multiple_of(MILLIS_PER_PLAYOUT_TICK)
        {
            return Err(PolicyError::PlayoutNotExactlyRepresentable);
        }
        let min_ticks =
            u16::try_from(min / MILLIS_PER_PLAYOUT_TICK).map_err(|_| PolicyError::PlayoutRange)?;
        let max_ticks =
            u16::try_from(max / MILLIS_PER_PLAYOUT_TICK).map_err(|_| PolicyError::PlayoutRange)?;
        Self::from_ticks(min_ticks, max_ticks)
    }

    pub const fn min(self) -> Duration {
        Duration::from_millis((self.min_ticks as u64).saturating_mul(MILLIS_PER_PLAYOUT_TICK))
    }

    pub const fn max(self) -> Duration {
        Duration::from_millis((self.max_ticks as u64).saturating_mul(MILLIS_PER_PLAYOUT_TICK))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PolicyError {
    PlayoutRange,
    PlayoutNotExactlyRepresentable,
    PriorityOutOfRange,
    BitrateOutOfRange,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DataChannelConfig {
    pub id: Option<DataChannelId>,
    pub label: Arc<str>,
    pub protocol: Arc<str>,
    pub ordered: bool,
    pub reliability: DataReliability,
    pub priority: DataChannelPriority,
    pub negotiated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataReliability {
    Reliable,
    MaxRetransmits(u16),
    MaxLifetime(Duration),
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DataChannelPriority(NonZeroU16);

impl DataChannelPriority {
    pub const VERY_LOW: Self = Self::new_const(128);
    pub const LOW: Self = Self::new_const(256);
    pub const MEDIUM: Self = Self::new_const(512);
    pub const HIGH: Self = Self::new_const(1024);

    #[allow(
        clippy::unreachable,
        reason = "all callers pass statically known nonzero priority constants"
    )]
    const fn new_const(weight: u16) -> Self {
        match NonZeroU16::new(weight) {
            Some(weight) => Self(weight),
            None => unreachable!(),
        }
    }

    pub fn new(weight: u16) -> Result<Self, DataChannelConfigError> {
        NonZeroU16::new(weight)
            .map(Self)
            .ok_or(DataChannelConfigError::PriorityOutOfRange)
    }

    pub const fn weight(self) -> u16 {
        self.0.get()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DataChannelConfigError {
    PriorityOutOfRange,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DataMessage {
    Text(bytes::Bytes),
    Binary(bytes::Bytes),
}
