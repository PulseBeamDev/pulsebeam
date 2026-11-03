#![allow(
    clippy::disallowed_types,
    reason = "the v3 public value contract uses immutable Arc-backed strings, slices, and Bytes"
)]

use std::{fmt, net::SocketAddr, num::NonZeroU16, sync::Arc, time::Duration};

use crate::id::SenderId;
use crate::{AcceptError, DataChannelId};

const MAX_PLAYOUT_TICKS: u16 = 4095;
const MILLIS_PER_PLAYOUT_TICK: u64 = 10;
const MAX_LOCAL_CANDIDATES: usize = 16;

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
pub enum CodecProfile {
    Opus,
    H264 {
        /// None means the offer omitted profile-level-id.
        profile_level_id: Option<u32>,
        packetization_mode: u8,
        level_asymmetry_allowed: bool,
    },
}

impl CodecProfile {
    pub const fn kind(self) -> MediaKind {
        match self {
            Self::Opus => MediaKind::Audio,
            Self::H264 { .. } => MediaKind::Video,
        }
    }
}

/// Immutable negotiated facts for one primary payload type and its repair map.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CodecInfo {
    pub payload_type: u8,
    pub profile: CodecProfile,
    pub clock_rate: u32,
    pub channels: Option<u8>,
    pub retransmission_payload_type: Option<u8>,
    pub(crate) nack: bool,
    pub(crate) twcc: bool,
    pub(crate) rfc8888: bool,
    pub(crate) pli: bool,
    pub(crate) fir: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacketFeedbackKind {
    TransportWide,
    Rfc8888,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaSectionKind {
    Audio,
    Video,
    Application,
}

/// Direction from the local connection's perspective.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MediaDirection {
    SendOnly,
    ReceiveOnly,
    Bidirectional,
    Inactive,
}

/// Every offered section, including disabled and non-RTP sections.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OfferedMediaSection {
    pub media_index: u32,
    pub mid: Arc<str>,
    pub kind: MediaSectionKind,
    pub direction: MediaDirection,
    /// Port-zero rejection, excluding BUNDLE-only sections.
    pub disabled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionInfo {
    pub offered_media: Arc<[OfferedMediaSection]>,
    pub feedback: Option<PacketFeedbackKind>,
    pub inbound_media: Arc<[InboundMediaInfo]>,
    pub senders: Arc<[SenderInfo]>,
}

impl SessionInfo {
    pub fn inbound_for_sender_index(&self, sender_index: u32) -> Option<&InboundMediaInfo> {
        self.inbound_media
            .iter()
            .find(|media| media.sender_index == sender_index)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InboundMediaInfo {
    pub sender_index: u32,
    pub kind: MediaKind,
    pub mid: Arc<str>,
    /// Incoming RID identities in offered simulcast order, including paused alternatives.
    /// Without simulcast, uses send-direction RID declaration order; empty means no RID.
    pub rids: Arc<[Arc<str>]>,
    /// Offered primary SSRCs, excluding FID repair members. These are signaling
    /// facts, not authenticated encoding discovery or an SSRC-to-RID binding.
    pub primary_ssrcs: Arc<[u32]>,
    pub codecs: Arc<[CodecInfo]>,
}

/// An outbound negotiated RTP extension mapping, independent of media semantics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RtpExtensionInfo {
    pub id: u8,
    pub uri: Arc<str>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SenderInfo {
    pub(crate) id: SenderId,
    pub receiver_index: u32,
    pub kind: MediaKind,
    pub mid: Arc<str>,
    pub codec: CodecInfo,
    /// Mappings the SFU uses to construct receiver-facing RTP extensions.
    pub extensions: Arc<[RtpExtensionInfo]>,
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

#[derive(Clone, Eq, PartialEq)]
pub struct LocalIceCredentials {
    ufrag: String,
    password: String,
}

impl LocalIceCredentials {
    pub fn new(ufrag: impl Into<String>, password: impl Into<String>) -> Result<Self, AcceptError> {
        let credentials = Self {
            ufrag: ufrag.into(),
            password: password.into(),
        };
        let ice_char = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/');
        if !(4..=256).contains(&credentials.ufrag.len())
            || !(22..=256).contains(&credentials.password.len())
            || !credentials.ufrag.bytes().all(ice_char)
            || !credentials.password.bytes().all(ice_char)
        {
            return Err(AcceptError::InvalidConfiguration);
        }
        Ok(credentials)
    }

    pub fn ufrag(&self) -> &str {
        &self.ufrag
    }

    pub(crate) fn password(&self) -> &str {
        &self.password
    }
}

impl fmt::Debug for LocalIceCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalIceCredentials")
            .field("ufrag", &self.ufrag)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConnectionLimits {
    /// Local per-message policy. Together with buffered bytes, the derived
    /// native receive window and send buffer must fit their u32 window fields.
    pub max_inbound_data_message_bytes: usize,
    /// Independent SCTP allowances for outbound caller payload bytes until native
    /// lifecycle completion and inbound completed-message Vec capacity until event
    /// dequeue, stale drop or abort. Caller-retained deliveries are outside scope.
    /// Inbound exhaustion resets the offending stream. Excludes native socket/
    /// reassembly buffers and event/index storage.
    pub max_buffered_data_bytes: usize,
    /// Local SCTP lifecycle-map key/value payloads, including empty sends, held
    /// until native lifecycle completion or map cleanup. Each admitted channel
    /// also prepays its reset-record and ready-set payloads until safe reset
    /// completion or cleanup; unknown-SID resets need independent credit.
    /// Includes channel-map payloads, config-lease payloads and label/protocol
    /// contents reserved per configuration, once across the channel and queued
    /// Opened event. Config credit lasts until the last native holder leaves;
    /// caller-retained deliveries are outside scope. Includes actual event-deque
    /// capacity: admission prepays Opened, Closed and one reset outcome; received
    /// messages cannot displace those notifications. Buffer credit lasts until
    /// idle with no future notifications, or abort. Independent of payload
    /// allowances. Excludes allocator/tree/Arc headers and native socket/
    /// reassembly; not an RSS bound.
    pub max_data_runtime_bytes: usize,
    /// For ingress, retained RTP Vec capacity, once across shared payload and
    /// extension slices; also the RTP/RTCP input-length maximum. Dequeue, drop
    /// or abort ends the ingress charge, not receiver retirement. Caller-retained
    /// delivery and event/string/extension-vector storage are excluded here;
    /// receive-runtime storage funds those owned metadata payloads and backing.
    pub max_queued_media_bytes: usize,
    /// Retained-original transport wire-byte sum, also a per-packet maximum.
    /// Independent of retained allocation/index payloads in send-runtime storage.
    pub max_retransmission_bytes: usize,
    /// Stored ingress facts/boxed payloads and encoding/RID group headers,
    /// with active/MID membership promises. Receiver/reservation tuples,
    /// per-media receiver memberships, NACK backing and owned RID/lookup-key
    /// payloads include future declared and paused receivers. Receiver
    /// retirement retains media/group storage until its media key is removed.
    /// Includes declared/observed SSRC ownership, declared-primary and repair-link
    /// index tuples. Learned repair ownership joins its receiver's lease until
    /// removal or transfer into declared media storage. Each issued SSRC reserves
    /// both ingress and transport permanent terminal-guard payloads until abort;
    /// transport active membership uses its future fence allowance until retirement.
    /// Includes actual event-deque
    /// capacity, with one future retirement-notice slot per receiver reservation,
    /// plus queued MID/RID/extension-URI string contents and actual extension-vector
    /// capacity until dequeue, drop or abort, not merely receiver retirement.
    /// Also funds actual incoming RTCP feedback/repair/keyframe deque capacities
    /// and owned TWCC/RFC 8888 report/status arrays until dequeue or abort.
    /// A selected receive-feedback profile prepays the actual receipt deque
    /// capacity before acceptance or late RTP capability declaration; history
    /// clear keeps that backing funded for reuse, while abort releases it.
    /// Excludes allocation/Arc header and tree overhead, parsing/staging copies, queued
    /// packet backing and replay state; not an RSS bound.
    pub max_receive_runtime_bytes: usize,
    /// Sender/facts/RID, live and promised sender indexes, grouped MID keys,
    /// permanent issued-SSRC guards and FIR/repair entries, plus retirement,
    /// media-queue and owned extension-vector backing, including spare capacity;
    /// retained-original Vec backing and cache record/group/expiry payloads,
    /// plus original plaintext workspace across working and pending ownership.
    /// Cleanup promises and jobs share funded retirement-buffer slots.
    /// Excludes shared Bytes/Arc, repair/padding workspace and transport packets,
    /// allocator/tree nodes, temporary growth/staging and other RTC storage;
    /// not an RSS bound.
    pub max_send_runtime_bytes: usize,
    /// Independent allowances for queued SCTP plaintext output and protected
    /// transport output, not an aggregate across stages. Each funds packet Vec
    /// capacity and physical queue backing. RTP/RTCP reserves before protection;
    /// unretainable emitted SCTP packets fail the association closed. Transfer
    /// ends that stage's packet charge; backing stays charged until released.
    /// Excludes caller-retained output, allocator overhead, temporary staging
    /// overlaps, native socket/reassembly, upstream crypto storage and pending
    /// inbound DTLS. This is not an RSS bound.
    pub max_transport_output_bytes: usize,
    /// Reserved/learned inbound RTP replay key/window payload bytes. Excludes
    /// allocator and index-node overhead; not a process-RSS bound.
    pub max_rtp_replay_state_bytes: usize,
    /// Authenticated SRTCP replay key/window payload bytes, accounted separately
    /// so RTP reservations cannot consume control's allowance.
    pub max_rtcp_replay_state_bytes: usize,
}

impl ConnectionLimits {
    /// Legacy compatibility value, no longer a source-admission policy.
    pub const DEFAULT_MAX_UNSIGNALED_ENCODINGS: u16 = 32;
    /// Legacy compatibility value, no longer a source-admission policy.
    pub const HARD_MAX_UNSIGNALED_ENCODINGS: u16 = 1024;
    /// Legacy compatibility value, no longer an RTC channel-admission policy.
    pub const DEFAULT_MAX_DATA_CHANNELS: u16 = 256;
    /// Legacy compatibility value matching the native u16 stream-count width,
    /// not a separate local channel-admission policy.
    pub const HARD_MAX_DATA_CHANNELS: u16 = u16::MAX;
    pub const DEFAULT_MAX_INBOUND_DATA_MESSAGE_BYTES: usize = 1024 * 1024;
    /// Legacy compatibility value, no longer a configuration-validation ceiling.
    pub const HARD_MAX_INBOUND_DATA_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
    pub const DEFAULT_MAX_BUFFERED_DATA_BYTES: usize = 8 * 1024 * 1024;
    /// Legacy compatibility value, no longer a configuration-validation ceiling.
    pub const HARD_MAX_BUFFERED_DATA_BYTES: usize = 64 * 1024 * 1024;
    pub const DEFAULT_MAX_DATA_RUNTIME_BYTES: usize = 64 * 1024 * 1024;
    pub const DEFAULT_MAX_QUEUED_MEDIA_BYTES: usize = 8 * 1024 * 1024;
    /// Legacy compatibility value, no longer a configuration-validation ceiling.
    pub const HARD_MAX_QUEUED_MEDIA_BYTES: usize = 64 * 1024 * 1024;
    pub const DEFAULT_MAX_RETRANSMISSION_BYTES: usize = 16 * 1024 * 1024;
    /// Legacy compatibility value, no longer a configuration-validation ceiling.
    pub const HARD_MAX_RETRANSMISSION_BYTES: usize = 128 * 1024 * 1024;
    pub const DEFAULT_MAX_RECEIVE_RUNTIME_BYTES: usize = 64 * 1024 * 1024;
    pub const DEFAULT_MAX_SEND_RUNTIME_BYTES: usize = 64 * 1024 * 1024;
    pub const DEFAULT_MAX_TRANSPORT_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
    pub const DEFAULT_MAX_RTP_REPLAY_STATE_BYTES: usize = 1024 * 1024;
    pub const DEFAULT_MAX_RTCP_REPLAY_STATE_BYTES: usize = 1024 * 1024;

    pub const fn validate(self) -> Result<Self, AcceptError> {
        if crate::sctp::buffer_limits(self).is_none() {
            return Err(AcceptError::InvalidConfiguration);
        }
        Ok(self)
    }
}

impl Default for ConnectionLimits {
    fn default() -> Self {
        Self {
            max_inbound_data_message_bytes: Self::DEFAULT_MAX_INBOUND_DATA_MESSAGE_BYTES,
            max_buffered_data_bytes: Self::DEFAULT_MAX_BUFFERED_DATA_BYTES,
            max_data_runtime_bytes: Self::DEFAULT_MAX_DATA_RUNTIME_BYTES,
            max_queued_media_bytes: Self::DEFAULT_MAX_QUEUED_MEDIA_BYTES,
            max_retransmission_bytes: Self::DEFAULT_MAX_RETRANSMISSION_BYTES,
            max_receive_runtime_bytes: Self::DEFAULT_MAX_RECEIVE_RUNTIME_BYTES,
            max_send_runtime_bytes: Self::DEFAULT_MAX_SEND_RUNTIME_BYTES,
            max_transport_output_bytes: Self::DEFAULT_MAX_TRANSPORT_OUTPUT_BYTES,
            max_rtp_replay_state_bytes: Self::DEFAULT_MAX_RTP_REPLAY_STATE_BYTES,
            max_rtcp_replay_state_bytes: Self::DEFAULT_MAX_RTCP_REPLAY_STATE_BYTES,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConnectionConfig {
    pub local_candidates: Vec<LocalCandidate>,
    pub local_ice_credentials: Option<LocalIceCredentials>,
    pub limits: ConnectionLimits,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlayoutDelay {
    min_millis: u16,
    max_millis: u16,
}

impl PlayoutDelay {
    pub const ZERO: Self = Self {
        min_millis: 0,
        max_millis: 0,
    };

    pub const fn from_ticks(min: u16, max: u16) -> Result<Self, PlayoutDelayError> {
        if min > max || max > MAX_PLAYOUT_TICKS {
            return Err(PlayoutDelayError::Range);
        }
        Ok(Self {
            min_millis: min.saturating_mul(10),
            max_millis: max.saturating_mul(10),
        })
    }

    pub fn from_millis(min: u64, max: u64) -> Result<Self, PlayoutDelayError> {
        if min > max || max > u64::from(MAX_PLAYOUT_TICKS).saturating_mul(MILLIS_PER_PLAYOUT_TICK) {
            return Err(PlayoutDelayError::Range);
        }
        Ok(Self {
            min_millis: u16::try_from(min).map_err(|_| PlayoutDelayError::Range)?,
            max_millis: u16::try_from(max).map_err(|_| PlayoutDelayError::Range)?,
        })
    }

    pub fn from_millis_exact(min: u64, max: u64) -> Result<Self, PlayoutDelayError> {
        if !min.is_multiple_of(MILLIS_PER_PLAYOUT_TICK)
            || !max.is_multiple_of(MILLIS_PER_PLAYOUT_TICK)
        {
            return Err(PlayoutDelayError::NotExactlyRepresentable);
        }
        let min_ticks =
            u16::try_from(min / MILLIS_PER_PLAYOUT_TICK).map_err(|_| PlayoutDelayError::Range)?;
        let max_ticks =
            u16::try_from(max / MILLIS_PER_PLAYOUT_TICK).map_err(|_| PlayoutDelayError::Range)?;
        Self::from_ticks(min_ticks, max_ticks)
    }

    pub const fn min(self) -> Duration {
        Duration::from_millis(self.min_millis as u64)
    }

    pub const fn max(self) -> Duration {
        Duration::from_millis(self.max_millis as u64)
    }

    /// Encode the explicitly requested bounds in 10 ms wire units, rounded to nearest.
    pub fn to_bytes(self) -> [u8; 3] {
        let min = u32::from(self.min_millis.saturating_add(5) / 10);
        let max = u32::from(self.max_millis.saturating_add(5) / 10);
        let [_, a, b, c] = ((min << 12) | max).to_be_bytes();
        [a, b, c]
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PlayoutDelayError {
    Range,
    NotExactlyRepresentable,
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
