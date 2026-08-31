use std::{
    cell::OnceCell,
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    net::SocketAddr,
    ops::Range,
    time::{Duration, Instant},
};
use str0m::rtp::ExtensionSerializer as _;

#[cfg(test)]
use std::cell::Cell;

use crate::{
    ChannelId, ConnectionId, DataChannelError, DataChannelEvent, DatagramProtocol, EgressDatagram,
    EgressSlot, IceCandidate, IceCredentials, IngressDatagram, IngressPacket, IngressStream,
    LiveConnection, LiveConnectionError, LocalTransport, MediaDirection, MediaKind, MediaSectionId,
    NegotiatedCodec, NegotiatedMedia, NegotiatedMediaSection, NegotiatedSession, PacketError,
    PacketId, PacketProvenance, PacketView, RtcNegotiation, SendId, ServerTransport,
    TransportEvent, TransportMetadata, negotiate,
};

use crate::egress::{
    EgressCodecConfig, EgressEngine, EgressLifecycle, EgressSlotConfig, ForwardAdmission,
    write_transport_sequence,
};

const MID_URI: &str = "urn:ietf:params:rtp-hdrext:sdes:mid";
const RID_URI: &str = "urn:ietf:params:rtp-hdrext:sdes:rtp-stream-id";
const AUDIO_LEVEL_URI: &str = "ssrc-audio-level";
const ABS_CAPTURE_TIME_URI: &str = "http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time";
const DEPENDENCY_DESCRIPTOR_URI: &str =
    "https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension";
const VIDEO_LAYERS_ALLOCATION_URI: &str =
    "http://www.webrtc.org/experiments/rtp-hdrext/video-layers-allocation00";
const MAX_EVENT_WORK: usize = 256;
const MAINTENANCE_PROBE_INTERVAL: Duration = Duration::from_secs(5);
const RECEIVER_REPORT_INTERVAL: Duration = Duration::from_secs(1);
const RECEIVER_REPORT_HISTORY: u64 = 8192;
const INGRESS_NACK_INTERVAL: Duration = Duration::from_millis(33);
const INGRESS_NACK_WINDOW: u64 = 100;
const INGRESS_NACK_ATTEMPTS: u8 = 5;
const REPAIRED_RID_URI: &str = "urn:ietf:params:rtp-hdrext:sdes:repaired-rtp-stream-id";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct ExtendedMediaSequence(u64);

impl ExtendedMediaSequence {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(transparent)]
pub struct ExtendedRtpTimestamp(u64);

impl ExtendedRtpTimestamp {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DependencyRewrite(Box<[u8]>);

impl DependencyRewrite {
    pub fn new(bytes: Box<[u8]>) -> Self {
        debug_assert!(!bytes.is_empty(), "a dependency rewrite contains bytes");
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaRewrite {
    pub sequence: ExtendedMediaSequence,
    pub timestamp: ExtendedRtpTimestamp,
    pub marker: bool,
    pub dependency: Option<DependencyRewrite>,
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum MediaPacketError {
    #[error("malformed H.264 payload")]
    MalformedH264,
    #[error("malformed Opus payload")]
    MalformedOpus,
    #[error("invalid absolute capture time extension")]
    InvalidAbsoluteCaptureTime,
    #[error("invalid audio level extension")]
    InvalidAudioLevel,
    #[error("invalid dependency descriptor extension")]
    InvalidDependencyDescriptor,
    #[error("invalid video layers allocation extension")]
    InvalidVideoLayersAllocation,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct H264NalMetadata {
    idr: bool,
    sps: bool,
    pps: bool,
    fragment_start: bool,
    fragment_end: bool,
}

impl H264NalMetadata {
    pub const fn idr(self) -> bool {
        self.idr
    }

    pub const fn sps(self) -> bool {
        self.sps
    }

    pub const fn pps(self) -> bool {
        self.pps
    }

    pub const fn fragment_start(self) -> bool {
        self.fragment_start
    }

    pub const fn fragment_end(self) -> bool {
        self.fragment_end
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaSemantics {
    keyframe: bool,
    frame_start: bool,
    h264: Option<H264NalMetadata>,
    opus_toc: Option<u8>,
}

impl MediaSemantics {
    pub const fn keyframe(self) -> bool {
        self.keyframe
    }

    pub const fn frame_start(self) -> bool {
        self.frame_start
    }

    pub const fn h264(self) -> Option<H264NalMetadata> {
        self.h264
    }

    pub const fn opus_toc(self) -> Option<u8> {
        self.opus_toc
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaExtensions<'a> {
    absolute_capture_time: Option<&'a [u8]>,
    audio_level: Option<i8>,
    dependency_descriptor: Option<&'a [u8]>,
    video_layers_allocation: Option<&'a VideoLayersAllocation>,
}

impl<'a> MediaExtensions<'a> {
    pub const fn absolute_capture_time(self) -> Option<&'a [u8]> {
        self.absolute_capture_time
    }

    pub const fn audio_level(self) -> Option<i8> {
        self.audio_level
    }

    pub const fn dependency_descriptor(self) -> Option<&'a [u8]> {
        self.dependency_descriptor
    }

    pub const fn video_layers_allocation(self) -> Option<&'a VideoLayersAllocation> {
        self.video_layers_allocation
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoLayersAllocation {
    current_stream: u8,
    streams: Vec<VideoStreamAllocation>,
}

impl VideoLayersAllocation {
    pub const fn current_stream(&self) -> u8 {
        self.current_stream
    }

    pub fn streams(&self) -> &[VideoStreamAllocation] {
        &self.streams
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoStreamAllocation {
    spatial_layers: Vec<VideoSpatialLayerAllocation>,
}

impl VideoStreamAllocation {
    pub fn spatial_layers(&self) -> &[VideoSpatialLayerAllocation] {
        &self.spatial_layers
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoSpatialLayerAllocation {
    cumulative_temporal_kbps: Vec<u64>,
    resolution: Option<(u16, u16, u8)>,
}

impl VideoSpatialLayerAllocation {
    pub fn cumulative_temporal_kbps(&self) -> &[u64] {
        &self.cumulative_temporal_kbps
    }

    pub const fn resolution(&self) -> Option<(u16, u16, u8)> {
        self.resolution
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct NegotiatedExtensionIds {
    absolute_capture_time: Option<u8>,
    audio_level: Option<u8>,
    dependency_descriptor: Option<u8>,
    video_layers_allocation: Option<u8>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ParsedMediaExtensions {
    absolute_capture_time: Option<Range<usize>>,
    audio_level: Option<i8>,
    dependency_descriptor: Option<Range<usize>>,
    video_layers_allocation: Option<VideoLayersAllocation>,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct MediaParseCounts {
    semantics: Cell<usize>,
    extensions: Cell<usize>,
}

#[derive(Debug)]
pub struct MediaPacket {
    bytes: Vec<u8>,
    stream: IngressStream,
    mid: String,
    rid: Option<String>,
    kind: MediaKind,
    codec: NegotiatedCodec,
    sequence: ExtendedMediaSequence,
    timestamp: ExtendedRtpTimestamp,
    marker: bool,
    payload: Range<usize>,
    received_at: Instant,
    packet_id: u64,
    extension_entries: Box<[crate::packet::RtpExtensionEntry]>,
    extension_ids: NegotiatedExtensionIds,
    semantics: OnceCell<Result<MediaSemantics, MediaPacketError>>,
    extensions: OnceCell<Result<ParsedMediaExtensions, MediaPacketError>>,
    #[cfg(test)]
    parse_counts: MediaParseCounts,
}

impl MediaPacket {
    pub const fn stream(&self) -> IngressStream {
        self.stream
    }

    pub fn mid(&self) -> &str {
        &self.mid
    }

    pub fn rid(&self) -> Option<&str> {
        self.rid.as_deref()
    }

    pub const fn kind(&self) -> MediaKind {
        self.kind
    }

    pub const fn codec(&self) -> &NegotiatedCodec {
        &self.codec
    }

    pub const fn sequence(&self) -> ExtendedMediaSequence {
        self.sequence
    }

    pub const fn timestamp(&self) -> ExtendedRtpTimestamp {
        self.timestamp
    }

    pub const fn marker(&self) -> bool {
        self.marker
    }

    pub fn payload(&self) -> &[u8] {
        self.bytes_at(self.payload.clone())
    }

    pub const fn received_at(&self) -> Instant {
        self.received_at
    }

    pub const fn packet_id(&self) -> u64 {
        self.packet_id
    }

    pub fn semantics(&self) -> Result<&MediaSemantics, MediaPacketError> {
        self.semantics
            .get_or_init(|| {
                #[cfg(test)]
                self.parse_counts
                    .semantics
                    .set(self.parse_counts.semantics.get().saturating_add(1));
                parse_media_semantics(&self.codec, self.payload())
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    pub fn extensions(&self) -> Result<MediaExtensions<'_>, MediaPacketError> {
        let parsed = self
            .extensions
            .get_or_init(|| {
                #[cfg(test)]
                self.parse_counts
                    .extensions
                    .set(self.parse_counts.extensions.get().saturating_add(1));
                parse_media_extensions(&self.bytes, &self.extension_entries, self.extension_ids)
            })
            .as_ref()
            .map_err(Clone::clone)?;
        Ok(MediaExtensions {
            absolute_capture_time: parsed
                .absolute_capture_time
                .clone()
                .map(|range| self.bytes_at(range)),
            audio_level: parsed.audio_level,
            dependency_descriptor: parsed
                .dependency_descriptor
                .clone()
                .map(|range| self.bytes_at(range)),
            video_layers_allocation: parsed.video_layers_allocation.as_ref(),
        })
    }

    fn bytes_at(&self, range: Range<usize>) -> &[u8] {
        let Some(bytes) = self.bytes.get(range) else {
            debug_assert!(false, "validated packet ranges remain within owned storage");
            return &[];
        };
        bytes
    }

    fn cached_clone_with_bytes(&self, bytes: Vec<u8>) -> Self {
        debug_assert_eq!(bytes.len(), self.bytes.len());
        let semantics = OnceCell::new();
        if let Some(value) = self.semantics.get() {
            let result = semantics.set(value.clone());
            debug_assert!(result.is_ok(), "a new semantic cache is empty");
        }
        let extensions = OnceCell::new();
        if let Some(value) = self.extensions.get() {
            let result = extensions.set(value.clone());
            debug_assert!(result.is_ok(), "a new extension cache is empty");
        }
        Self {
            bytes,
            stream: self.stream,
            mid: self.mid.clone(),
            rid: self.rid.clone(),
            kind: self.kind,
            codec: self.codec.clone(),
            sequence: self.sequence,
            timestamp: self.timestamp,
            marker: self.marker,
            payload: self.payload.clone(),
            received_at: self.received_at,
            packet_id: self.packet_id,
            extension_entries: self.extension_entries.clone(),
            extension_ids: self.extension_ids,
            semantics,
            extensions,
            #[cfg(test)]
            parse_counts: MediaParseCounts::default(),
        }
    }

    #[cfg(test)]
    fn parse_count_values(&self) -> (usize, usize) {
        (
            self.parse_counts.semantics.get(),
            self.parse_counts.extensions.get(),
        )
    }
}

#[derive(Debug)]
pub struct TransitMediaPacket(MediaPacket);

impl TransitMediaPacket {
    pub fn materialize(packet: &MediaPacket) -> Self {
        let bytes = packet.bytes.to_vec();
        debug_assert_ne!(bytes.as_ptr(), packet.bytes.as_ptr());
        Self(packet.cached_clone_with_bytes(bytes))
    }

    pub const fn packet(&self) -> &MediaPacket {
        &self.0
    }
}

fn parse_media_semantics(
    codec: &NegotiatedCodec,
    payload: &[u8],
) -> Result<MediaSemantics, MediaPacketError> {
    if codec.name.eq_ignore_ascii_case("h264") {
        return parse_h264_semantics(payload);
    }
    if codec.name.eq_ignore_ascii_case("opus") {
        let toc = *payload.first().ok_or(MediaPacketError::MalformedOpus)?;
        return Ok(MediaSemantics {
            keyframe: true,
            frame_start: true,
            h264: None,
            opus_toc: Some(toc),
        });
    }
    Ok(MediaSemantics {
        keyframe: false,
        frame_start: true,
        h264: None,
        opus_toc: None,
    })
}

fn parse_h264_semantics(payload: &[u8]) -> Result<MediaSemantics, MediaPacketError> {
    let first = *payload.first().ok_or(MediaPacketError::MalformedH264)?;
    let packet_type = first & 0x1f;
    let mut nal = H264NalMetadata::default();
    let frame_start = match packet_type {
        1..=23 => {
            mark_h264_type(&mut nal, packet_type);
            true
        }
        24 => {
            let mut offset = 1usize;
            while offset < payload.len() {
                let length_end = offset
                    .checked_add(2)
                    .ok_or(MediaPacketError::MalformedH264)?;
                let length = payload
                    .get(offset..length_end)
                    .and_then(|value| value.try_into().ok())
                    .map(u16::from_be_bytes)
                    .map(usize::from)
                    .ok_or(MediaPacketError::MalformedH264)?;
                if length == 0 {
                    return Err(MediaPacketError::MalformedH264);
                }
                let nal_start = length_end;
                let nal_end = nal_start
                    .checked_add(length)
                    .ok_or(MediaPacketError::MalformedH264)?;
                let nal_type = payload
                    .get(nal_start..nal_end)
                    .and_then(|value| value.first())
                    .map(|value| value & 0x1f)
                    .ok_or(MediaPacketError::MalformedH264)?;
                mark_h264_type(&mut nal, nal_type);
                offset = nal_end;
            }
            true
        }
        28 => {
            let fu = *payload.get(1).ok_or(MediaPacketError::MalformedH264)?;
            mark_h264_type(&mut nal, fu & 0x1f);
            nal.fragment_start = fu & 0x80 != 0;
            nal.fragment_end = fu & 0x40 != 0;
            nal.idr &= nal.fragment_start;
            nal.fragment_start
        }
        _ => true,
    };
    Ok(MediaSemantics {
        keyframe: nal.idr,
        frame_start,
        h264: Some(nal),
        opus_toc: None,
    })
}

fn mark_h264_type(metadata: &mut H264NalMetadata, nal_type: u8) {
    match nal_type {
        5 => metadata.idr = true,
        7 => metadata.sps = true,
        8 => metadata.pps = true,
        _ => {}
    }
}

fn parse_media_extensions(
    bytes: &[u8],
    entries: &[crate::packet::RtpExtensionEntry],
    ids: NegotiatedExtensionIds,
) -> Result<ParsedMediaExtensions, MediaPacketError> {
    let absolute_capture_time = ids
        .absolute_capture_time
        .and_then(|id| extension_range(entries, id));
    if absolute_capture_time
        .as_ref()
        .is_some_and(|range| !matches!(range.len(), 8 | 16))
    {
        return Err(MediaPacketError::InvalidAbsoluteCaptureTime);
    }
    let audio_level =
        if let Some(range) = ids.audio_level.and_then(|id| extension_range(entries, id)) {
            let value = bytes
                .get(range)
                .and_then(|value| value.first())
                .copied()
                .ok_or(MediaPacketError::InvalidAudioLevel)?;
            i8::try_from(value & 0x7f).ok().and_then(i8::checked_neg)
        } else {
            None
        };
    let dependency_descriptor = ids
        .dependency_descriptor
        .and_then(|id| extension_range(entries, id));
    if dependency_descriptor.as_ref().is_some_and(Range::is_empty) {
        return Err(MediaPacketError::InvalidDependencyDescriptor);
    }
    let video_layers_allocation = ids
        .video_layers_allocation
        .and_then(|id| extension_range(entries, id))
        .map(|range| {
            bytes
                .get(range)
                .and_then(parse_video_layers_allocation)
                .ok_or(MediaPacketError::InvalidVideoLayersAllocation)
        })
        .transpose()?;
    Ok(ParsedMediaExtensions {
        absolute_capture_time,
        audio_level,
        dependency_descriptor,
        video_layers_allocation,
    })
}

fn parse_video_layers_allocation(bytes: &[u8]) -> Option<VideoLayersAllocation> {
    let mut values = str0m::rtp::ExtensionValues::default();
    if !str0m::rtp::vla::Serializer.parse_value(bytes, &mut values) {
        return None;
    }
    let parsed = values
        .user_values
        .get::<str0m::rtp::vla::VideoLayersAllocation>()?;
    debug_assert!(parsed.simulcast_streams.len() <= 5);
    debug_assert!(
        parsed
            .simulcast_streams
            .iter()
            .all(|stream| stream.spatial_layers.len() <= 4)
    );
    debug_assert!(parsed.simulcast_streams.iter().all(|stream| {
        stream
            .spatial_layers
            .iter()
            .all(|spatial| spatial.temporal_layers.len() <= 5)
    }));
    Some(VideoLayersAllocation {
        current_stream: parsed.current_simulcast_stream_index,
        streams: parsed
            .simulcast_streams
            .iter()
            .map(|stream| VideoStreamAllocation {
                spatial_layers: stream
                    .spatial_layers
                    .iter()
                    .map(|spatial| VideoSpatialLayerAllocation {
                        cumulative_temporal_kbps: spatial
                            .temporal_layers
                            .iter()
                            .map(|temporal| temporal.cumulative_kbps)
                            .collect(),
                        resolution: spatial
                            .resolution_and_framerate
                            .as_ref()
                            .map(|value| (value.width, value.height, value.framerate)),
                    })
                    .collect(),
            })
            .collect(),
    })
}

fn extension_range(entries: &[crate::packet::RtpExtensionEntry], id: u8) -> Option<Range<usize>> {
    entries
        .iter()
        .find(|entry| entry.id() == id)
        .map(crate::packet::RtpExtensionEntry::value)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct DataChannel(u16);

impl DataChannel {
    const fn from_id(id: ChannelId) -> Self {
        Self(id.get())
    }

    const fn id(self) -> ChannelId {
        ChannelId::new(self.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DataChannelMode {
    ReliableOrdered,
    UnreliableUnordered,
    Unsupported,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DataPayload {
    Text(String),
    Binary(Vec<u8>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DataBackpressure {
    channel: DataChannel,
}

impl DataBackpressure {
    pub const fn channel(self) -> DataChannel {
        self.channel
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BweCapacity {
    bitrate_bps: u64,
    observed_at: Instant,
}

impl BweCapacity {
    pub const fn bitrate_bps(self) -> u64 {
        self.bitrate_bps
    }

    pub const fn observed_at(self) -> Instant {
        self.observed_at
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RtcConnectionState {
    Negotiated,
    Connecting,
    Connected,
    Draining,
    Closed,
    Failed,
}

#[allow(
    clippy::large_enum_variant,
    reason = "boxing every authenticated media event would add a packet-path allocation"
)]
#[derive(Debug)]
pub enum RtcEvent {
    ConnectionStateChanged(RtcConnectionState),
    Media(MediaPacket),
    KeyframeRequested(EgressSlot),
    BweCapacity(BweCapacity),
    DataChannelOpened {
        channel: DataChannel,
        label: String,
        protocol: String,
        mode: DataChannelMode,
    },
    DataMessage {
        channel: DataChannel,
        payload: DataPayload,
    },
    DataChannelClosed(DataChannel),
    DataChannelReady,
    DataChannelUnavailable,
    DataBackpressure(DataBackpressure),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct DepartureReceipt(u64);

#[derive(Debug, PartialEq, Eq)]
pub struct Transmit {
    protocol: DatagramProtocol,
    source: SocketAddr,
    destination: SocketAddr,
    bytes: Vec<u8>,
    receipt: DepartureReceipt,
}

impl Transmit {
    pub const fn protocol(&self) -> DatagramProtocol {
        self.protocol
    }

    pub const fn source(&self) -> SocketAddr {
        self.source
    }

    pub const fn destination(&self) -> SocketAddr {
        self.destination
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub const fn receipt(&self) -> DepartureReceipt {
        self.receipt
    }

    pub fn into_parts(
        self,
    ) -> (
        DatagramProtocol,
        SocketAddr,
        SocketAddr,
        Vec<u8>,
        DepartureReceipt,
    ) {
        (
            self.protocol,
            self.source,
            self.destination,
            self.bytes,
            self.receipt,
        )
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RtcPeerError {
    #[error("invalid local ICE credentials")]
    InvalidIceCredentials,
    #[error("invalid local ICE candidate")]
    InvalidCandidate,
    #[error("RTC negotiation failed: {0}")]
    Negotiation(String),
    #[error("RTC transport failed: {0}")]
    Transport(String),
    #[error("RTC media operation failed: {0}")]
    Media(String),
    #[error("RTC data operation failed: {0}")]
    Data(String),
    #[error("RTC data channel is backpressured")]
    DataBackpressure(DataBackpressure),
    #[error("unknown ingress stream")]
    UnknownIngress,
    #[error("unknown egress slot")]
    UnknownEgress,
    #[error("media packet is incompatible with the egress slot")]
    IncompatibleMedia,
    #[error("media rewrite is invalid")]
    InvalidRewrite,
    #[error("unknown or completed departure receipt")]
    UnknownDepartureReceipt,
    #[error("RTC peer is closed")]
    Closed,
}

impl From<LiveConnectionError> for RtcPeerError {
    fn from(error: LiveConnectionError) -> Self {
        Self::Transport(error.to_string())
    }
}

impl From<PacketError> for RtcPeerError {
    fn from(error: PacketError) -> Self {
        Self::Media(error.to_string())
    }
}

impl From<MediaPacketError> for RtcPeerError {
    fn from(error: MediaPacketError) -> Self {
        Self::Media(error.to_string())
    }
}

#[derive(Clone)]
struct IngressFacts {
    stream: IngressStream,
    section: MediaSectionId,
    rid: Option<String>,
}

#[derive(Clone)]
struct EgressFacts {
    section: MediaSectionId,
    ssrc: u32,
    rtx_ssrc: Option<u32>,
    kind: MediaKind,
    codecs: Box<[String]>,
}

struct BuiltMediaFacts {
    public: Box<[NegotiatedMedia]>,
    ingress: Box<[IngressFacts]>,
    egress: HashMap<EgressSlot, EgressFacts>,
}

#[derive(Clone, Copy)]
struct PendingDeparture {
    send_id: Option<SendId>,
    congestion_tracked: bool,
    lifecycle: Option<EgressLifecycle>,
}

#[derive(Clone, Copy, Default)]
struct SequenceExtender {
    highest: Option<u64>,
}

impl SequenceExtender {
    fn extend(&mut self, sequence: u16) -> u64 {
        let Some(highest) = self.highest else {
            let extended = u64::from(sequence);
            self.highest = Some(extended);
            return extended;
        };
        let cycle = highest & !u64::from(u16::MAX);
        let mut extended = cycle | u64::from(sequence);
        if extended.saturating_add(1 << 15) < highest {
            extended = extended.saturating_add(1 << 16);
        } else if extended > highest.saturating_add(1 << 15) {
            extended = extended.saturating_sub(1 << 16);
        }
        if extended > highest {
            self.highest = Some(extended);
        }
        extended
    }
}

#[derive(Clone, Copy, Default)]
struct TimestampExtender {
    highest: Option<u64>,
}

impl TimestampExtender {
    fn extend(&mut self, timestamp: u32) -> u64 {
        let Some(highest) = self.highest else {
            let extended = u64::from(timestamp);
            self.highest = Some(extended);
            return extended;
        };
        let cycle = highest & !u64::from(u32::MAX);
        let mut extended = cycle | u64::from(timestamp);
        if extended.saturating_add(1 << 31) < highest {
            extended = extended.saturating_add(1 << 32);
        } else if extended > highest.saturating_add(1 << 31) {
            extended = extended.saturating_sub(1 << 32);
        }
        if extended > highest {
            self.highest = Some(extended);
        }
        extended
    }
}

#[derive(Default)]
struct IngressNackRegister {
    highest: Option<u64>,
    missing: BTreeMap<u64, u8>,
}

impl IngressNackRegister {
    fn observe(&mut self, sequence: u64) -> bool {
        let Some(highest) = self.highest else {
            self.highest = Some(sequence);
            return true;
        };
        if sequence > highest {
            let first_missing = highest
                .saturating_add(1)
                .max(sequence.saturating_sub(INGRESS_NACK_WINDOW));
            for missing in first_missing..sequence {
                self.missing.entry(missing).or_insert(0);
            }
            self.highest = Some(sequence);
            let oldest = sequence.saturating_sub(INGRESS_NACK_WINDOW);
            self.missing.retain(|missing, _| *missing >= oldest);
            return true;
        }
        self.missing.remove(&sequence).is_some()
    }

    fn report(&mut self) -> Vec<u16> {
        self.missing
            .iter_mut()
            .filter_map(|(sequence, attempts)| {
                if *attempts >= INGRESS_NACK_ATTEMPTS {
                    return None;
                }
                *attempts = attempts.saturating_add(1);
                Some(u16::try_from(*sequence & u64::from(u16::MAX)).unwrap_or(u16::MAX))
            })
            .collect()
    }

    fn has_pending(&self) -> bool {
        self.missing
            .values()
            .any(|attempts| *attempts < INGRESS_NACK_ATTEMPTS)
    }
}

struct IngressRecovery {
    media_ssrc: u32,
    register: IngressNackRegister,
}

struct ReceiverReportRegister {
    base_sequence: u64,
    highest_sequence: u64,
    received: u64,
    expected_prior: u64,
    received_prior: u64,
    seen: HashSet<u64>,
    last_transit: Option<i64>,
    jitter: u64,
    last_sender_report: Option<(u32, Instant)>,
}

#[derive(Clone, Copy)]
struct ReceiverReportBlock {
    source_ssrc: u32,
    fraction_lost: u8,
    cumulative_lost: i32,
    extended_highest_sequence: u32,
    jitter: u32,
    last_sender_report: u32,
    delay_since_sender_report: u32,
}

impl ReceiverReportRegister {
    fn new(sequence: u64) -> Self {
        Self {
            base_sequence: sequence,
            highest_sequence: sequence,
            received: 0,
            expected_prior: 0,
            received_prior: 0,
            seen: HashSet::with_capacity(64),
            last_transit: None,
            jitter: 0,
            last_sender_report: None,
        }
    }

    fn observe(
        &mut self,
        sequence: u64,
        timestamp: u64,
        received_at: Instant,
        epoch: Instant,
        clock_rate: u32,
    ) {
        debug_assert!(clock_rate > 0, "negotiated RTP clock rates are nonzero");
        if sequence.saturating_add(RECEIVER_REPORT_HISTORY) < self.highest_sequence
            || !self.seen.insert(sequence)
        {
            return;
        }
        self.received = self.received.saturating_add(1);
        self.highest_sequence = self.highest_sequence.max(sequence);
        let oldest = self
            .highest_sequence
            .saturating_sub(RECEIVER_REPORT_HISTORY);
        self.seen.retain(|seen| *seen >= oldest);

        let arrival_micros = received_at.saturating_duration_since(epoch).as_micros();
        let arrival_ticks = arrival_micros
            .saturating_mul(u128::from(clock_rate))
            .saturating_div(1_000_000);
        let arrival_ticks = i64::try_from(arrival_ticks).unwrap_or(i64::MAX);
        let timestamp = i64::try_from(timestamp).unwrap_or(i64::MAX);
        let transit = arrival_ticks.saturating_sub(timestamp);
        if let Some(previous) = self.last_transit {
            let deviation = transit.abs_diff(previous);
            if deviation >= self.jitter {
                self.jitter = self
                    .jitter
                    .saturating_add(deviation.saturating_sub(self.jitter) / 16);
            } else {
                self.jitter = self
                    .jitter
                    .saturating_sub(self.jitter.saturating_sub(deviation) / 16);
            }
        }
        self.last_transit = Some(transit);
    }

    fn observe_sender_report(&mut self, compact_ntp: u32, received_at: Instant) {
        self.last_sender_report = Some((compact_ntp, received_at));
    }

    fn report(&mut self, source_ssrc: u32, now: Instant) -> ReceiverReportBlock {
        let expected = self
            .highest_sequence
            .saturating_sub(self.base_sequence)
            .saturating_add(1);
        let expected_interval = expected.saturating_sub(self.expected_prior);
        let received_interval = self.received.saturating_sub(self.received_prior);
        let lost_interval = expected_interval.saturating_sub(received_interval);
        let fraction_lost = if expected_interval == 0 || lost_interval == 0 {
            0
        } else {
            u8::try_from(
                lost_interval
                    .saturating_mul(256)
                    .checked_div(expected_interval)
                    .unwrap_or(0)
                    .min(255),
            )
            .unwrap_or(u8::MAX)
        };
        self.expected_prior = expected;
        self.received_prior = self.received;
        let cumulative_lost = i32::try_from(
            i64::try_from(expected)
                .unwrap_or(i64::MAX)
                .saturating_sub(i64::try_from(self.received).unwrap_or(i64::MAX))
                .clamp(-0x80_0000, 0x7f_ffff),
        )
        .unwrap_or(0);
        let (last_sender_report, delay_since_sender_report) =
            self.last_sender_report
                .map_or((0, 0), |(compact_ntp, received_at)| {
                    let delay = now.saturating_duration_since(received_at).as_micros();
                    let delay = delay.saturating_mul(65_536).saturating_div(1_000_000);
                    (compact_ntp, u32::try_from(delay).unwrap_or(u32::MAX))
                });
        ReceiverReportBlock {
            source_ssrc,
            fraction_lost,
            cumulative_lost,
            extended_highest_sequence: u32::try_from(self.highest_sequence & u64::from(u32::MAX))
                .unwrap_or(u32::MAX),
            jitter: u32::try_from(self.jitter).unwrap_or(u32::MAX),
            last_sender_report,
            delay_since_sender_report,
        }
    }
}

pub struct RtcPeer {
    connection: LiveConnection,
    media_egress: EgressEngine,
    state: RtcConnectionState,
    state_events: VecDeque<RtcConnectionState>,
    media_events: VecDeque<RtcEvent>,
    pending_capacity: Option<BweCapacity>,
    last_now: Instant,
    next_packet_id: u64,
    next_receipt: u64,
    desired_bitrate_bps: u64,
    current_bitrate_bps: u64,
    ingress: Box<[IngressFacts]>,
    ingress_by_ssrc: HashMap<u32, IngressStream>,
    primary_ssrc_by_stream: HashMap<IngressStream, u32>,
    ingress_recovery: HashMap<u32, IngressRecovery>,
    next_ingress_nack: Option<Instant>,
    receiver_reports: HashMap<u32, ReceiverReportRegister>,
    receiver_report_epoch: Instant,
    next_receiver_report: Option<Instant>,
    source_sequences: HashMap<u32, SequenceExtender>,
    source_timestamps: HashMap<u32, TimestampExtender>,
    egress: HashMap<EgressSlot, EgressFacts>,
    pending_departures: HashMap<DepartureReceipt, PendingDeparture>,
    next_maintenance_probe: Option<Instant>,
    #[cfg(test)]
    last_forward_storage: Option<usize>,
}

impl RtcPeer {
    #[allow(
        clippy::too_many_arguments,
        reason = "accepting a peer requires the complete small set of signaling and transport facts"
    )]
    pub fn accept(
        now: Instant,
        connection_id: u64,
        offer: &str,
        ice_ufrag: String,
        ice_password: String,
        local_candidates: Box<[String]>,
    ) -> Result<(Self, RtcNegotiation), RtcPeerError> {
        let ice = IceCredentials::new(ice_ufrag, ice_password)
            .ok_or(RtcPeerError::InvalidIceCredentials)?;
        let local = LocalTransport::generate(ice.clone())?;
        let candidates = local_candidates
            .into_vec()
            .into_iter()
            .map(|candidate| IceCandidate::new(candidate).ok_or(RtcPeerError::InvalidCandidate))
            .collect::<Result<Vec<_>, _>>()?
            .into_boxed_slice();
        let server =
            ServerTransport::new(connection_id, ice, local.fingerprint().clone(), candidates);
        let negotiated = negotiate(offer, &server)
            .map_err(|error| RtcPeerError::Negotiation(error.to_string()))?;
        let session = negotiated.session().clone();
        let BuiltMediaFacts {
            public,
            ingress,
            egress,
        } = build_media_facts(connection_id, &session)?;
        let mut egress_configs = Vec::with_capacity(egress.len());
        for (slot, slot_facts) in &egress {
            let section = session
                .media_section(slot_facts.section)
                .ok_or(RtcPeerError::UnknownEgress)?;
            egress_configs.push(egress_slot_config(*slot, slot_facts, section));
        }
        let connection =
            LiveConnection::new(ConnectionId::new(connection_id), session, local, now)?;
        let media_egress = EgressEngine::new(now, egress_configs);
        let answer = RtcNegotiation {
            answer: negotiated.answer().as_str().to_owned(),
            media: public,
        };
        Ok((
            Self {
                connection,
                media_egress,
                state: RtcConnectionState::Negotiated,
                state_events: VecDeque::from([RtcConnectionState::Negotiated]),
                media_events: VecDeque::new(),
                pending_capacity: None,
                last_now: now,
                next_packet_id: 0,
                next_receipt: 0,
                desired_bitrate_bps: 0,
                current_bitrate_bps: 0,
                ingress,
                ingress_by_ssrc: HashMap::new(),
                primary_ssrc_by_stream: HashMap::new(),
                ingress_recovery: HashMap::new(),
                next_ingress_nack: None,
                receiver_reports: HashMap::new(),
                receiver_report_epoch: now,
                next_receiver_report: None,
                source_sequences: HashMap::new(),
                source_timestamps: HashMap::new(),
                egress,
                pending_departures: HashMap::new(),
                next_maintenance_probe: None,
                #[cfg(test)]
                last_forward_storage: None,
            },
            answer,
        ))
    }

    pub const fn state(&self) -> RtcConnectionState {
        self.state
    }

    pub fn handle_datagram(
        &mut self,
        now: Instant,
        datagram: IngressDatagram,
    ) -> Result<(), RtcPeerError> {
        self.ensure_open()?;
        self.observe_now(now);
        let provenance = PacketProvenance::new(
            now,
            TransportMetadata::new(
                datagram.protocol.into_transport(),
                datagram.source,
                datagram.destination,
            ),
            PacketId::new(self.next_packet_id),
        );
        self.next_packet_id = self.next_packet_id.wrapping_add(1);
        self.connection
            .handle_datagram(now, IngressPacket::new(&datagram.bytes, provenance))?;
        if self
            .connection
            .next_deadline(now)
            .is_some_and(|deadline| deadline <= now)
        {
            self.connection.handle_timeout(now);
        }
        Ok(())
    }

    pub fn handle_timeout(&mut self, now: Instant) -> Result<(), RtcPeerError> {
        self.ensure_open()?;
        self.observe_now(now);
        self.connection.handle_timeout(now);
        debug_assert!(
            self.connection
                .next_deadline(now)
                .is_none_or(|deadline| deadline > now),
            "transport deadlines advance after timeout handling"
        );
        self.send_ingress_nacks(now)?;
        self.send_receiver_reports(now)?;
        self.maybe_request_maintenance_probe(now);
        self.drain_congestion();
        Ok(())
    }

    pub fn next_deadline(&mut self) -> Option<Instant> {
        if matches!(
            self.state,
            RtcConnectionState::Closed | RtcConnectionState::Failed
        ) {
            return None;
        }
        let connection_deadline = self.connection.next_deadline(self.last_now);
        let media_deadline = self
            .connection
            .media_egress_ready()
            .then(|| self.media_egress.next_ready(self.last_now))
            .flatten();
        let mut deadline = connection_deadline;
        if let Some(ready) = media_deadline {
            debug_assert!(
                ready > self.last_now || self.connection.media_egress_ready(),
                "immediately ready media requires a writable transport"
            );
            deadline = Some(deadline.map_or(ready, |current| current.min(ready)));
        }
        if let Some(probe) = self.next_maintenance_probe {
            deadline = Some(deadline.map_or(probe, |current| current.min(probe)));
        }
        if let Some(nack) = self.next_ingress_nack {
            deadline = Some(deadline.map_or(nack, |current| current.min(nack)));
        }
        if let Some(report) = self.next_receiver_report {
            deadline = Some(deadline.map_or(report, |current| current.min(report)));
        }
        let deadline = deadline.map(|deadline| deadline.max(self.last_now));
        debug_assert!(
            deadline.is_none_or(|deadline| deadline >= self.last_now),
            "RTC deadline cannot precede the observed clock: connection={connection_deadline:?}, media={media_deadline:?}, probe={:?}, nack={:?}, report={:?}, now={:?}",
            self.next_maintenance_probe,
            self.next_ingress_nack,
            self.next_receiver_report,
            self.last_now
        );
        deadline
    }

    pub fn poll_event(&mut self) -> Option<RtcEvent> {
        if let Some(state) = self.state_events.pop_front() {
            return Some(RtcEvent::ConnectionStateChanged(state));
        }
        if let Some(event) = self.media_events.pop_front() {
            return Some(event);
        }
        self.drain_congestion();
        if let Some(capacity) = self.pending_capacity.take() {
            return Some(RtcEvent::BweCapacity(capacity));
        }
        for _ in 0..MAX_EVENT_WORK {
            if let Some(event) = self.connection.poll_event() {
                if let Some(event) = self.normalize_transport_event(event) {
                    return Some(event);
                }
                continue;
            }
            if let Some(event) = self
                .connection
                .data_association()
                .and_then(crate::DataChannelAssociation::poll_event)
            {
                return Some(self.normalize_data_event(event));
            }
            if let Some(authenticated) = self.connection.poll_authenticated() {
                if let Some(event) = self.normalize_authenticated(authenticated) {
                    return Some(event);
                }
                if let Some(event) = self.media_events.pop_front() {
                    return Some(event);
                }
                continue;
            }
            self.drain_congestion();
            if let Some(capacity) = self.pending_capacity.take() {
                return Some(RtcEvent::BweCapacity(capacity));
            }
            return None;
        }
        debug_assert!(false, "one RTC event poll exceeded its bounded work budget");
        None
    }

    pub fn forward(
        &mut self,
        now: Instant,
        slot: EgressSlot,
        packet: &MediaPacket,
        rewrite: MediaRewrite,
    ) -> Result<(), RtcPeerError> {
        self.forward_packet(now, slot, packet, rewrite)
    }

    pub fn forward_transit(
        &mut self,
        now: Instant,
        slot: EgressSlot,
        packet: &TransitMediaPacket,
        rewrite: MediaRewrite,
    ) -> Result<(), RtcPeerError> {
        self.forward_packet(now, slot, &packet.0, rewrite)
    }

    pub fn request_keyframe(
        &mut self,
        now: Instant,
        source: IngressStream,
    ) -> Result<(), RtcPeerError> {
        self.ensure_open()?;
        self.observe_now(now);
        let media_ssrc = self
            .primary_ssrc_by_stream
            .get(&source)
            .copied()
            .ok_or(RtcPeerError::UnknownIngress)?;
        let sender_ssrc = u32::try_from(self.connection.id().get())
            .unwrap_or(u32::MAX)
            .max(1);
        let mut pli = [0u8; 12];
        pli[0] = 0x81;
        pli[1] = 206;
        pli[2..4].copy_from_slice(&2u16.to_be_bytes());
        pli[4..8].copy_from_slice(&sender_ssrc.to_be_bytes());
        pli[8..12].copy_from_slice(&media_ssrc.to_be_bytes());
        self.connection.send_rtcp(&pli)?;
        Ok(())
    }

    pub fn set_desired_bitrate(
        &mut self,
        now: Instant,
        bitrate_bps: u64,
    ) -> Result<(), RtcPeerError> {
        self.ensure_open()?;
        self.observe_now(now);
        self.desired_bitrate_bps = bitrate_bps;
        if let Some(outcome) = self
            .connection
            .set_max_total_allocated_bitrate(now, bitrate_bps)
        {
            self.apply_congestion(outcome);
        }
        self.update_maintenance_deadline(now);
        Ok(())
    }

    pub fn set_current_bitrate(
        &mut self,
        now: Instant,
        bitrate_bps: u64,
    ) -> Result<(), RtcPeerError> {
        self.ensure_open()?;
        self.observe_now(now);
        self.current_bitrate_bps = bitrate_bps;
        self.update_maintenance_deadline(now);
        Ok(())
    }

    pub fn send_data(
        &mut self,
        now: Instant,
        channel: DataChannel,
        payload: DataPayload,
    ) -> Result<(), RtcPeerError> {
        self.ensure_open()?;
        self.observe_now(now);
        let Some(association) = self.connection.data_association() else {
            return Err(RtcPeerError::Data(
                "data channels were not negotiated".to_owned(),
            ));
        };
        let (binary, bytes) = match payload {
            DataPayload::Text(value) => (false, value.into_bytes()),
            DataPayload::Binary(value) => (true, value),
        };
        association
            .send(channel.id(), binary, bytes)
            .map_err(|error| match error {
                DataChannelError::EgressFull => {
                    RtcPeerError::DataBackpressure(DataBackpressure { channel })
                }
                error => RtcPeerError::Data(error.to_string()),
            })?;
        self.connection.drive_data(now);
        Ok(())
    }

    pub fn poll_transmit(&mut self, now: Instant) -> Option<Transmit> {
        if matches!(
            self.state,
            RtcConnectionState::Closed | RtcConnectionState::Failed
        ) {
            return None;
        }
        self.observe_now(now);
        self.maybe_request_maintenance_probe(now);
        self.drain_congestion();
        self.connection.drive_data(now);
        if let Some(datagram) = self.connection.poll_egress() {
            return Some(self.wrap_transmit(datagram, None));
        }
        if !self.connection.media_egress_ready() {
            return None;
        }
        self.media_egress.ensure_probe_fallback(now);
        let mut ready = self.media_egress.poll_ready(now)?;
        let congestion_tracked = if let Some(offset) = ready.twcc_offset {
            let Ok(congestion) =
                self.connection
                    .assign_congestion(ready.send_id, ready.bytes.len(), ready.probe_id)
            else {
                debug_assert!(false, "eligible RTP has unique congestion identity");
                return None;
            };
            if !write_transport_sequence(&mut ready.bytes, offset, congestion.transport_sequence())
            {
                debug_assert!(false, "negotiated TWCC offset remains valid");
                return None;
            }
            if self
                .connection
                .send_rtp_with_assigned_congestion(
                    &ready.bytes,
                    ready.extended_sequence,
                    ready.send_id,
                )
                .is_err()
            {
                debug_assert!(false, "eligible RTP can be protected and queued");
                return None;
            }
            true
        } else {
            if self
                .connection
                .send_rtp(&ready.bytes, ready.extended_sequence)
                .is_err()
            {
                debug_assert!(false, "eligible RTP can be protected and queued");
                return None;
            }
            false
        };
        if let Some(probe_id) = ready.completed_probe {
            self.connection.complete_probe(probe_id);
        }
        let datagram = self.connection.poll_egress();
        debug_assert!(datagram.is_some(), "queued RTP produces one datagram");
        let datagram = datagram?;
        debug_assert_eq!(
            datagram.congestion_tracked(),
            congestion_tracked,
            "the protected datagram preserves congestion ownership"
        );
        Some(self.wrap_transmit(datagram, ready.lifecycle))
    }

    pub fn confirm_departure(
        &mut self,
        receipt: DepartureReceipt,
        at: Instant,
    ) -> Result<Option<ForwardingLatency>, RtcPeerError> {
        self.observe_now(at);
        let pending = self
            .pending_departures
            .remove(&receipt)
            .ok_or(RtcPeerError::UnknownDepartureReceipt)?;
        if pending.congestion_tracked
            && let Some(send_id) = pending.send_id
        {
            self.connection.report_departure(send_id, at)?;
        }
        Ok(pending.lifecycle.map(|lifecycle| {
            let latency = forwarding_latency(lifecycle, at);
            record_forwarding_latency(latency);
            latency
        }))
    }

    pub fn abandon_departure(&mut self, receipt: DepartureReceipt) -> Result<(), RtcPeerError> {
        self.pending_departures
            .remove(&receipt)
            .ok_or(RtcPeerError::UnknownDepartureReceipt)?;
        Ok(())
    }

    pub fn close(&mut self, now: Instant) {
        self.observe_now(now);
        if matches!(
            self.state,
            RtcConnectionState::Closed | RtcConnectionState::Failed
        ) {
            return;
        }
        self.transition(RtcConnectionState::Draining);
        self.pending_departures.clear();
        self.transition(RtcConnectionState::Closed);
    }

    fn forward_packet(
        &mut self,
        now: Instant,
        slot: EgressSlot,
        packet: &MediaPacket,
        rewrite: MediaRewrite,
    ) -> Result<(), RtcPeerError> {
        self.ensure_open()?;
        self.observe_now(now);
        if rewrite
            .dependency
            .as_ref()
            .is_some_and(|dependency| dependency.as_bytes().is_empty())
        {
            return Err(RtcPeerError::InvalidRewrite);
        }
        let destination = self.validate_forward(slot, packet)?;
        #[cfg(test)]
        {
            self.last_forward_storage = Some(packet.bytes.as_ptr() as usize);
        }
        debug_assert_eq!(
            destination.section,
            self.egress
                .get(&slot)
                .map(|facts| facts.section)
                .unwrap_or(destination.section)
        );
        let extensions = packet.extensions()?;
        let dependency_descriptor = rewrite
            .dependency
            .as_ref()
            .map(DependencyRewrite::as_bytes)
            .or_else(|| extensions.dependency_descriptor());
        self.media_egress
            .admit(
                slot,
                ForwardAdmission {
                    codec: packet.codec.name(),
                    logical_sequence: rewrite.sequence.get(),
                    timestamp: rewrite.timestamp.get(),
                    marker: rewrite.marker,
                    payload: packet.payload(),
                    absolute_capture_time: extensions.absolute_capture_time(),
                    audio_level: extensions.audio_level(),
                    dependency_descriptor,
                    ingress_at: packet.received_at,
                    admitted_at: now,
                },
            )
            .map_err(|()| RtcPeerError::Media("egress media queue is full".to_owned()))
    }

    fn validate_forward(
        &self,
        slot: EgressSlot,
        packet: &MediaPacket,
    ) -> Result<EgressFacts, RtcPeerError> {
        let destination = self
            .egress
            .get(&slot)
            .cloned()
            .ok_or(RtcPeerError::UnknownEgress)?;
        if destination.kind != packet.kind
            || !destination
                .codecs
                .iter()
                .any(|codec| codec.eq_ignore_ascii_case(packet.codec.name()))
        {
            return Err(RtcPeerError::IncompatibleMedia);
        }
        Ok(destination)
    }

    fn normalize_authenticated(
        &mut self,
        authenticated: crate::AuthenticatedPacket,
    ) -> Option<RtcEvent> {
        let parsed = authenticated.parse().ok()?;
        match parsed {
            PacketView::Rtp(packet) => {
                let extension_entries = packet.extension_entries().ok()?;
                if let Some(transport_sequence) = transport_sequence_before_media_demux(
                    self.connection.session(),
                    &extension_entries,
                    packet.bytes(),
                ) {
                    self.connection.observe_transport_sequence(
                        transport_sequence,
                        packet.provenance().received_at(),
                        packet.ssrc(),
                    );
                }
                if packet.ssrc() == 0 {
                    return None;
                }
                let (
                    section_id,
                    mid,
                    rid,
                    kind,
                    codec,
                    extension_ids,
                    retransmission,
                    recovery_enabled,
                ) = {
                    let section = self.known_ingress_section(packet.ssrc()).or_else(|| {
                        resolve_ingress_section(
                            self.connection.session(),
                            packet.payload_type(),
                            &extension_entries,
                            packet.bytes(),
                        )
                    })?;
                    let extension_ids = negotiated_extension_ids(section);
                    let codec = section.codecs().iter().find(|codec| {
                        codec.payload_type() == packet.payload_type()
                            || codec.retransmission_payload_type() == Some(packet.payload_type())
                    })?;
                    let retransmission =
                        codec.retransmission_payload_type() == Some(packet.payload_type());
                    let rid = if retransmission {
                        extension_ids.repaired_rid
                    } else {
                        extension_ids.rid
                    }
                    .and_then(|id| extension_text(&extension_entries, packet.bytes(), id));
                    (
                        section.id(),
                        section.mid().to_owned(),
                        rid,
                        section.kind(),
                        NegotiatedCodec::from(codec),
                        extension_ids.media,
                        retransmission,
                        codec.nack() && codec.retransmission_payload_type().is_some(),
                    )
                };
                let stream =
                    self.resolve_ingress_stream(section_id, rid.as_deref(), packet.ssrc())?;
                let (media_ssrc, wire_sequence, payload) = if retransmission {
                    let payload = packet.payload();
                    let original = payload
                        .get(..2)
                        .and_then(|value| value.try_into().ok())
                        .map(u16::from_be_bytes)?;
                    let primary_ssrc = self.primary_ssrc_by_stream.get(&stream).copied()?;
                    let payload_range = packet.payload_range();
                    let payload_start = payload_range.start.checked_add(2)?;
                    debug_assert!(payload_start <= payload_range.end);
                    (primary_ssrc, original, payload_start..payload_range.end)
                } else {
                    self.observe_primary_ssrc(stream, packet.ssrc(), recovery_enabled);
                    (
                        packet.ssrc(),
                        packet.sequence_number(),
                        packet.payload_range(),
                    )
                };
                let sequence = self
                    .source_sequences
                    .entry(media_ssrc)
                    .or_default()
                    .extend(wire_sequence);
                let timestamp = self
                    .source_timestamps
                    .entry(media_ssrc)
                    .or_default()
                    .extend(packet.timestamp());
                self.observe_receiver_report(
                    media_ssrc,
                    sequence,
                    timestamp,
                    packet.provenance().received_at(),
                    codec.clock_rate(),
                );
                if !self.observe_ingress_sequence(media_ssrc, sequence, recovery_enabled) {
                    return None;
                }
                if retransmission {
                    metrics::counter!("rtc_ingress_rtx_recovered").increment(1);
                }
                let fields = (
                    stream,
                    mid,
                    rid,
                    kind,
                    codec,
                    ExtendedMediaSequence::new(sequence),
                    ExtendedRtpTimestamp::new(timestamp),
                    packet.marker(),
                    payload,
                    packet.provenance().received_at(),
                    packet.provenance().packet_id().get(),
                    extension_entries,
                    extension_ids,
                );
                let (bytes, _) = authenticated.into_parts();
                Some(RtcEvent::Media(MediaPacket {
                    bytes,
                    stream: fields.0,
                    mid: fields.1,
                    rid: fields.2,
                    kind: fields.3,
                    codec: fields.4,
                    sequence: fields.5,
                    timestamp: fields.6,
                    marker: fields.7,
                    payload: fields.8,
                    received_at: fields.9,
                    packet_id: fields.10,
                    extension_entries: fields.11,
                    extension_ids: fields.12,
                    semantics: OnceCell::new(),
                    extensions: OnceCell::new(),
                    #[cfg(test)]
                    parse_counts: MediaParseCounts::default(),
                }))
            }
            PacketView::Rtcp(packet) => {
                let received_at = authenticated.provenance().received_at();
                for rtcp in packet.packets() {
                    let Ok(Some(report)) = rtcp.sender_report() else {
                        continue;
                    };
                    let compact_ntp =
                        u32::try_from((report.ntp_timestamp() >> 16) & u64::from(u32::MAX))
                            .unwrap_or(u32::MAX);
                    if let Some(register) = self.receiver_reports.get_mut(&report.ssrc()) {
                        register.observe_sender_report(compact_ntp, received_at);
                    }
                }
                if let Ok(nacks) = packet.nacks() {
                    for nack in nacks {
                        self.media_egress.handle_nack(
                            nack.media_ssrc(),
                            nack.sequences(),
                            authenticated.provenance().received_at(),
                        );
                    }
                }
                for rtcp in packet.packets() {
                    let Ok(Some(feedback)) = rtcp.feedback() else {
                        continue;
                    };
                    if feedback.media_ssrc() == 0 {
                        continue;
                    }
                    if feedback.packet_type() == 206
                        && matches!(feedback.format(), 1 | 4)
                        && let Some(slot) = self.media_egress.slot_for_ssrc(feedback.media_ssrc())
                    {
                        self.media_events
                            .push_back(RtcEvent::KeyframeRequested(slot));
                    }
                }
                None
            }
        }
    }

    fn resolve_ingress_stream(
        &mut self,
        section: MediaSectionId,
        rid: Option<&str>,
        ssrc: u32,
    ) -> Option<IngressStream> {
        if let Some(stream) = self.ingress_by_ssrc.get(&ssrc).copied() {
            return Some(stream);
        }
        let facts = self.ingress.iter().find(|facts| {
            facts.section == section
                && (facts.rid.as_deref() == rid || rid.is_none() || facts.rid.is_none())
        })?;
        let stream = facts.stream;
        let previous = self.ingress_by_ssrc.insert(ssrc, stream);
        debug_assert!(
            previous.is_none(),
            "new SSRCs have no previous stream mapping"
        );
        Some(stream)
    }

    fn known_ingress_section(&self, ssrc: u32) -> Option<&NegotiatedMediaSection> {
        let stream = self.ingress_by_ssrc.get(&ssrc)?;
        let section = self
            .ingress
            .iter()
            .find(|facts| facts.stream == *stream)?
            .section;
        self.connection.session().media_section(section)
    }

    fn observe_primary_ssrc(
        &mut self,
        stream: IngressStream,
        media_ssrc: u32,
        recovery_enabled: bool,
    ) {
        let previous = self.primary_ssrc_by_stream.insert(stream, media_ssrc);
        if previous.is_some_and(|previous| previous != media_ssrc) {
            let previous = previous.expect("checked as present");
            self.ingress_recovery.remove(&previous);
            self.source_sequences.remove(&previous);
            self.source_timestamps.remove(&previous);
            self.receiver_reports.remove(&previous);
        }
        if recovery_enabled {
            self.ingress_recovery
                .entry(media_ssrc)
                .or_insert_with(|| IngressRecovery {
                    media_ssrc,
                    register: IngressNackRegister::default(),
                });
        }
    }

    fn observe_ingress_sequence(
        &mut self,
        media_ssrc: u32,
        sequence: u64,
        recovery_enabled: bool,
    ) -> bool {
        if !recovery_enabled {
            return true;
        }
        let Some(recovery) = self.ingress_recovery.get_mut(&media_ssrc) else {
            debug_assert!(false, "NACK-enabled ingress has a primary SSRC register");
            return false;
        };
        let accepted = recovery.register.observe(sequence);
        if recovery.register.has_pending() && self.next_ingress_nack.is_none() {
            self.next_ingress_nack = self.last_now.checked_add(INGRESS_NACK_INTERVAL);
        }
        accepted
    }

    fn send_ingress_nacks(&mut self, now: Instant) -> Result<(), RtcPeerError> {
        if self.next_ingress_nack.is_none_or(|deadline| deadline > now) {
            return Ok(());
        }
        let sender_ssrc = u32::try_from(self.connection.id().get())
            .unwrap_or(u32::MAX)
            .max(1);
        let reports = self
            .ingress_recovery
            .values_mut()
            .filter_map(|recovery| {
                let missing = recovery.register.report();
                (!missing.is_empty()).then_some((recovery.media_ssrc, missing))
            })
            .collect::<Vec<_>>();
        for (media_ssrc, missing) in reports {
            metrics::counter!("rtc_ingress_nack_requested")
                .increment(u64::try_from(missing.len()).unwrap_or(u64::MAX));
            self.connection
                .send_rtcp(&encode_nack(sender_ssrc, media_ssrc, &missing))?;
        }
        self.next_ingress_nack = self
            .ingress_recovery
            .values()
            .any(|recovery| recovery.register.has_pending())
            .then(|| now.checked_add(INGRESS_NACK_INTERVAL))
            .flatten();
        Ok(())
    }

    fn observe_receiver_report(
        &mut self,
        media_ssrc: u32,
        sequence: u64,
        timestamp: u64,
        received_at: Instant,
        clock_rate: u32,
    ) {
        debug_assert_ne!(media_ssrc, 0, "SSRC 0 never creates media receiver state");
        match self.receiver_reports.entry(media_ssrc) {
            std::collections::hash_map::Entry::Occupied(mut entry) => entry.get_mut().observe(
                sequence,
                timestamp,
                received_at,
                self.receiver_report_epoch,
                clock_rate,
            ),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let mut register = ReceiverReportRegister::new(sequence);
                register.observe(
                    sequence,
                    timestamp,
                    received_at,
                    self.receiver_report_epoch,
                    clock_rate,
                );
                entry.insert(register);
            }
        }
        if self.next_receiver_report.is_none() {
            self.next_receiver_report = received_at.checked_add(RECEIVER_REPORT_INTERVAL);
        }
    }

    fn send_receiver_reports(&mut self, now: Instant) -> Result<(), RtcPeerError> {
        if self
            .next_receiver_report
            .is_none_or(|deadline| deadline > now)
        {
            return Ok(());
        }
        let sender_ssrc = u32::try_from(self.connection.id().get())
            .unwrap_or(u32::MAX)
            .max(1);
        let mut blocks = self
            .receiver_reports
            .iter_mut()
            .map(|(source_ssrc, register)| register.report(*source_ssrc, now))
            .collect::<Vec<_>>();
        blocks.sort_unstable_by_key(|report| report.source_ssrc);
        for reports in blocks.chunks(31) {
            self.connection
                .send_rtcp(&encode_receiver_report(sender_ssrc, reports))?;
        }
        self.next_receiver_report = (!self.receiver_reports.is_empty())
            .then(|| now.checked_add(RECEIVER_REPORT_INTERVAL))
            .flatten();
        Ok(())
    }

    fn normalize_transport_event(&mut self, event: TransportEvent) -> Option<RtcEvent> {
        let state = match event {
            TransportEvent::IceChecking | TransportEvent::IceConnected => {
                RtcConnectionState::Connecting
            }
            TransportEvent::DtlsConnected => {
                if let Some(data) = self.connection.data_association() {
                    data.connect(self.last_now);
                    self.connection.drive_data(self.last_now);
                }
                RtcConnectionState::Connected
            }
            TransportEvent::IceDisconnected | TransportEvent::DtlsClosed => {
                RtcConnectionState::Closed
            }
            TransportEvent::IceFailed => RtcConnectionState::Failed,
        };
        if self.set_state(state) {
            Some(RtcEvent::ConnectionStateChanged(state))
        } else {
            None
        }
    }

    fn normalize_data_event(&self, event: DataChannelEvent) -> RtcEvent {
        match event {
            DataChannelEvent::Open(open) => RtcEvent::DataChannelOpened {
                channel: DataChannel::from_id(open.id()),
                label: open.label().to_owned(),
                protocol: open.protocol().to_owned(),
                mode: if open.reliability().ordered()
                    && open.reliability().max_retransmits_value().is_none()
                    && open.reliability().max_lifetime_value().is_none()
                {
                    DataChannelMode::ReliableOrdered
                } else if !open.reliability().ordered()
                    && open.reliability().max_retransmits_value() == Some(0)
                    && open.reliability().max_lifetime_value().is_none()
                {
                    DataChannelMode::UnreliableUnordered
                } else {
                    DataChannelMode::Unsupported
                },
            },
            DataChannelEvent::Message {
                id,
                binary,
                payload,
            } => RtcEvent::DataMessage {
                channel: DataChannel::from_id(id),
                payload: if binary {
                    DataPayload::Binary(payload)
                } else {
                    DataPayload::Text(String::from_utf8_lossy(&payload).into_owned())
                },
            },
            DataChannelEvent::Close(id) => RtcEvent::DataChannelClosed(DataChannel::from_id(id)),
            DataChannelEvent::AssociationConnected => RtcEvent::DataChannelReady,
            DataChannelEvent::AssociationClosed | DataChannelEvent::Error => {
                RtcEvent::DataChannelUnavailable
            }
        }
    }

    fn drain_congestion(&mut self) {
        while let Some(outcome) = self.connection.poll_congestion() {
            self.apply_congestion(outcome);
        }
    }

    fn apply_congestion(&mut self, outcome: crate::GccOutcome) {
        self.media_egress
            .set_pacing_rate(self.last_now, outcome.pacing_bitrate_bps());
        if let Some(probe) = outcome.probe() {
            self.media_egress.start_probe(self.last_now, probe);
        }
        self.pending_capacity = Some(BweCapacity {
            bitrate_bps: outcome.estimate().bitrate_bps(),
            observed_at: self.last_now,
        });
    }

    fn update_maintenance_deadline(&mut self, now: Instant) {
        self.next_maintenance_probe = if self.desired_bitrate_bps > self.current_bitrate_bps
            && self.desired_bitrate_bps > 0
        {
            self.next_maintenance_probe
                .or_else(|| now.checked_add(MAINTENANCE_PROBE_INTERVAL))
        } else {
            None
        };
    }

    fn maybe_request_maintenance_probe(&mut self, now: Instant) {
        let Some(deadline) = self.next_maintenance_probe else {
            return;
        };
        if now < deadline {
            return;
        }
        if !self.media_egress.has_active_probe()
            && let Some(outcome) = self
                .connection
                .request_maintenance_probe(now, self.desired_bitrate_bps)
        {
            self.apply_congestion(outcome);
        }
        self.next_maintenance_probe = now.checked_add(MAINTENANCE_PROBE_INTERVAL);
    }

    fn wrap_transmit(
        &mut self,
        datagram: EgressDatagram,
        lifecycle: Option<EgressLifecycle>,
    ) -> Transmit {
        let (bytes, transport, send_id, congestion_tracked) = datagram.into_parts();
        let receipt = DepartureReceipt(self.next_receipt);
        self.next_receipt = self.next_receipt.wrapping_add(1);
        let previous = self.pending_departures.insert(
            receipt,
            PendingDeparture {
                send_id,
                congestion_tracked,
                lifecycle,
            },
        );
        debug_assert!(
            previous.is_none(),
            "uncompleted receipt identifiers do not wrap"
        );
        Transmit {
            protocol: DatagramProtocol::from_transport(transport.protocol()),
            source: transport.source(),
            destination: transport.destination(),
            bytes,
            receipt,
        }
    }

    fn ensure_open(&self) -> Result<(), RtcPeerError> {
        if matches!(
            self.state,
            RtcConnectionState::Closed | RtcConnectionState::Failed
        ) {
            Err(RtcPeerError::Closed)
        } else {
            Ok(())
        }
    }

    fn observe_now(&mut self, now: Instant) {
        debug_assert!(
            now >= self.last_now,
            "RTC time is monotonic: now={now:?}, last={:?}",
            self.last_now
        );
        self.last_now = self.last_now.max(now);
    }

    fn transition(&mut self, state: RtcConnectionState) {
        if self.set_state(state) {
            self.state_events.push_back(state);
        }
    }

    fn set_state(&mut self, state: RtcConnectionState) -> bool {
        if self.state == state {
            return false;
        }
        debug_assert!(
            valid_state_transition(self.state, state),
            "RTC lifecycle transition is valid"
        );
        self.state = state;
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ForwardingLatency {
    service: Duration,
    pacing: Duration,
    egress: Duration,
    total: Duration,
}

impl ForwardingLatency {
    pub const fn service(self) -> Duration {
        self.service
    }

    pub const fn pacing(self) -> Duration {
        self.pacing
    }

    pub const fn egress(self) -> Duration {
        self.egress
    }

    pub const fn total(self) -> Duration {
        self.total
    }
}

fn forwarding_latency(lifecycle: EgressLifecycle, departed_at: Instant) -> ForwardingLatency {
    debug_assert!(lifecycle.admitted_at >= lifecycle.ingress_at);
    debug_assert!(lifecycle.eligible_at >= lifecycle.admitted_at);
    debug_assert!(departed_at >= lifecycle.eligible_at);
    ForwardingLatency {
        service: lifecycle
            .admitted_at
            .saturating_duration_since(lifecycle.ingress_at),
        pacing: lifecycle
            .eligible_at
            .saturating_duration_since(lifecycle.admitted_at),
        egress: departed_at.saturating_duration_since(lifecycle.eligible_at),
        total: departed_at.saturating_duration_since(lifecycle.ingress_at),
    }
}

fn record_forwarding_latency(latency: ForwardingLatency) {
    metrics::histogram!("forwarding_service_us").record(latency.service.as_micros() as f64);
    metrics::histogram!("forwarding_pacing_us").record(latency.pacing.as_micros() as f64);
    metrics::histogram!("forwarding_egress_lateness_us").record(latency.egress.as_micros() as f64);
    metrics::histogram!("forwarding_total_us").record(latency.total.as_micros() as f64);
}

const fn valid_state_transition(from: RtcConnectionState, to: RtcConnectionState) -> bool {
    matches!(
        (from, to),
        (
            RtcConnectionState::Negotiated,
            RtcConnectionState::Connecting
        ) | (
            RtcConnectionState::Negotiated,
            RtcConnectionState::Connected
        ) | (RtcConnectionState::Negotiated, RtcConnectionState::Draining)
            | (RtcConnectionState::Negotiated, RtcConnectionState::Closed)
            | (RtcConnectionState::Negotiated, RtcConnectionState::Failed)
            | (
                RtcConnectionState::Connecting,
                RtcConnectionState::Connected
            )
            | (RtcConnectionState::Connecting, RtcConnectionState::Draining)
            | (RtcConnectionState::Connecting, RtcConnectionState::Closed)
            | (RtcConnectionState::Connecting, RtcConnectionState::Failed)
            | (
                RtcConnectionState::Connected,
                RtcConnectionState::Connecting
            )
            | (RtcConnectionState::Connected, RtcConnectionState::Draining)
            | (RtcConnectionState::Connected, RtcConnectionState::Closed)
            | (RtcConnectionState::Connected, RtcConnectionState::Failed)
            | (RtcConnectionState::Draining, RtcConnectionState::Closed)
            | (RtcConnectionState::Draining, RtcConnectionState::Failed)
    )
}

fn build_media_facts(
    connection_id: u64,
    session: &NegotiatedSession,
) -> Result<BuiltMediaFacts, RtcPeerError> {
    let mut media = Vec::new();
    let mut ingress = Vec::new();
    let mut egress = HashMap::new();
    let mut used_ssrcs = HashSet::new();
    let mut next_ingress = 1u32;
    let mut next_egress = 1u32;
    for section in session.media_sections() {
        if section.kind() == MediaKind::Application {
            media.push(negotiated_media(section, None, None, None));
            continue;
        }
        match section.direction() {
            MediaDirection::ReceiveOnly => {
                if section.receive_rids().is_empty() {
                    let stream = IngressStream::new(next_ingress);
                    next_ingress = next_ingress
                        .checked_add(1)
                        .ok_or(RtcPeerError::UnknownIngress)?;
                    ingress.push(IngressFacts {
                        stream,
                        section: section.id(),
                        rid: None,
                    });
                    media.push(negotiated_media(section, Some(stream), None, None));
                } else {
                    for rid in section.receive_rids() {
                        let stream = IngressStream::new(next_ingress);
                        next_ingress = next_ingress
                            .checked_add(1)
                            .ok_or(RtcPeerError::UnknownIngress)?;
                        ingress.push(IngressFacts {
                            stream,
                            section: section.id(),
                            rid: Some(rid.clone()),
                        });
                        media.push(negotiated_media(
                            section,
                            Some(stream),
                            None,
                            Some(rid.clone()),
                        ));
                    }
                }
            }
            MediaDirection::SendOnly => {
                let slot = EgressSlot::new(next_egress);
                next_egress = next_egress
                    .checked_add(1)
                    .ok_or(RtcPeerError::UnknownEgress)?;
                let ssrc = allocate_ssrc(connection_id, slot, &mut used_ssrcs);
                let rtx_ssrc = section
                    .codecs()
                    .iter()
                    .any(|codec| codec.retransmission_payload_type().is_some())
                    .then(|| allocate_related_ssrc(ssrc, &mut used_ssrcs));
                egress.insert(
                    slot,
                    EgressFacts {
                        section: section.id(),
                        ssrc,
                        rtx_ssrc,
                        kind: section.kind(),
                        codecs: section
                            .codecs()
                            .iter()
                            .map(|codec| codec.name().to_owned())
                            .collect::<Vec<_>>()
                            .into_boxed_slice(),
                    },
                );
                media.push(negotiated_media(section, None, Some(slot), None));
            }
            MediaDirection::Inactive | MediaDirection::Bidirectional => {
                media.push(negotiated_media(section, None, None, None));
            }
        }
    }
    Ok(BuiltMediaFacts {
        public: media.into_boxed_slice(),
        ingress: ingress.into_boxed_slice(),
        egress,
    })
}

fn negotiated_media(
    section: &NegotiatedMediaSection,
    ingress: Option<IngressStream>,
    egress: Option<EgressSlot>,
    rid: Option<String>,
) -> NegotiatedMedia {
    NegotiatedMedia {
        ingress,
        egress,
        mid: section.mid().to_owned(),
        rid,
        kind: section.kind(),
        direction: section.direction(),
        codecs: section
            .codecs()
            .iter()
            .map(NegotiatedCodec::from)
            .collect::<Vec<_>>()
            .into_boxed_slice(),
    }
}

fn allocate_ssrc(connection_id: u64, slot: EgressSlot, used: &mut HashSet<u32>) -> u32 {
    let low = u32::try_from(connection_id & u64::from(u32::MAX)).unwrap_or(u32::MAX);
    let mut candidate = low.rotate_left(13).wrapping_add(slot.0).max(1);
    while candidate == 0 || !used.insert(candidate) {
        candidate = candidate.wrapping_add(1).max(1);
    }
    candidate
}

fn allocate_related_ssrc(primary: u32, used: &mut HashSet<u32>) -> u32 {
    let mut candidate = primary.rotate_left(16).wrapping_add(0x9e37_79b9).max(1);
    while candidate == 0 || !used.insert(candidate) {
        candidate = candidate.wrapping_add(1).max(1);
    }
    candidate
}

fn egress_slot_config(
    slot: EgressSlot,
    facts: &EgressFacts,
    section: &NegotiatedMediaSection,
) -> EgressSlotConfig {
    debug_assert_eq!(facts.section, section.id());
    EgressSlotConfig {
        slot,
        kind: facts.kind,
        mid: section.mid().as_bytes().into(),
        primary_ssrc: facts.ssrc,
        rtx_ssrc: facts.rtx_ssrc,
        codecs: section
            .codecs()
            .iter()
            .map(|codec| EgressCodecConfig {
                name: codec.name().into(),
                primary_payload_type: codec.payload_type(),
                rtx_payload_type: codec.retransmission_payload_type(),
            })
            .collect::<Vec<_>>()
            .into_boxed_slice(),
        mid_extension: extension_id(section, |uri| uri == MID_URI),
        twcc_extension: extension_id(section, |uri| uri.contains("transport-wide-cc")),
        absolute_capture_time_extension: extension_id(section, |uri| uri == ABS_CAPTURE_TIME_URI),
        audio_level_extension: extension_id(section, |uri| uri.contains(AUDIO_LEVEL_URI)),
        dependency_descriptor_extension: extension_id(section, |uri| {
            uri == DEPENDENCY_DESCRIPTOR_URI || uri.contains("dependency-descriptor")
        }),
    }
}

fn resolve_ingress_section<'a>(
    session: &'a NegotiatedSession,
    payload_type: u8,
    entries: &[crate::packet::RtpExtensionEntry],
    bytes: &[u8],
) -> Option<&'a NegotiatedMediaSection> {
    let mut fallback = None;
    for section in session.media_sections().iter().filter(|section| {
        section.direction() == MediaDirection::ReceiveOnly
            && section.codecs().iter().any(|codec| {
                codec.payload_type() == payload_type
                    || codec.retransmission_payload_type() == Some(payload_type)
            })
    }) {
        let packet_mid = extension_id(section, |uri| uri == MID_URI)
            .and_then(|id| extension_text(entries, bytes, id));
        if packet_mid.as_deref() == Some(section.mid()) {
            return Some(section);
        }
        if packet_mid.is_none() {
            if fallback.is_some() {
                fallback = None;
                break;
            }
            fallback = Some(section);
        }
    }
    fallback
}

fn transport_sequence_before_media_demux(
    session: &NegotiatedSession,
    entries: &[crate::packet::RtpExtensionEntry],
    bytes: &[u8],
) -> Option<u16> {
    session
        .media_sections()
        .iter()
        .filter(|section| section.direction() == MediaDirection::ReceiveOnly)
        .flat_map(NegotiatedMediaSection::header_extensions)
        .filter(|extension| extension.uri().contains("transport-wide-cc"))
        .filter_map(|extension| extension_value(entries, bytes, extension.id()))
        .find_map(|value| value.try_into().ok().map(u16::from_be_bytes))
}

#[derive(Clone, Copy)]
struct SectionExtensionIds {
    rid: Option<u8>,
    repaired_rid: Option<u8>,
    media: NegotiatedExtensionIds,
}

fn negotiated_extension_ids(section: &NegotiatedMediaSection) -> SectionExtensionIds {
    SectionExtensionIds {
        rid: extension_id(section, |uri| uri == RID_URI),
        repaired_rid: extension_id(section, |uri| uri == REPAIRED_RID_URI),
        media: NegotiatedExtensionIds {
            absolute_capture_time: extension_id(section, |uri| uri == ABS_CAPTURE_TIME_URI),
            audio_level: extension_id(section, |uri| uri.contains(AUDIO_LEVEL_URI)),
            dependency_descriptor: extension_id(section, |uri| {
                uri == DEPENDENCY_DESCRIPTOR_URI || uri.contains("dependency-descriptor")
            }),
            video_layers_allocation: extension_id(section, |uri| {
                uri == VIDEO_LAYERS_ALLOCATION_URI
            }),
        },
    }
}

#[allow(
    clippy::indexing_slicing,
    reason = "the fixed header and bounded 24-byte report blocks are allocated before encoding"
)]
fn encode_receiver_report(sender_ssrc: u32, reports: &[ReceiverReportBlock]) -> Vec<u8> {
    debug_assert!(
        !reports.is_empty(),
        "a receiver report contains source state"
    );
    debug_assert!(reports.len() <= 31, "RTCP report count is five bits");
    let packet_len = 8usize.saturating_add(reports.len().saturating_mul(24));
    debug_assert!(packet_len.is_multiple_of(4));
    let mut packet = vec![0u8; packet_len];
    packet[0] = 0x80 | u8::try_from(reports.len()).unwrap_or(31);
    packet[1] = 201;
    let words = packet_len.saturating_div(4).saturating_sub(1);
    packet[2..4].copy_from_slice(&u16::try_from(words).unwrap_or(u16::MAX).to_be_bytes());
    packet[4..8].copy_from_slice(&sender_ssrc.to_be_bytes());
    for (index, report) in reports.iter().enumerate() {
        let start = 8usize.saturating_add(index.saturating_mul(24));
        let end = start.saturating_add(24);
        let Some(block) = packet.get_mut(start..end) else {
            debug_assert!(false, "bounded report blocks fit their encoded packet");
            break;
        };
        block[0..4].copy_from_slice(&report.source_ssrc.to_be_bytes());
        block[4] = report.fraction_lost;
        let lost = report.cumulative_lost.to_be_bytes();
        block[5..8].copy_from_slice(&lost[1..4]);
        block[8..12].copy_from_slice(&report.extended_highest_sequence.to_be_bytes());
        block[12..16].copy_from_slice(&report.jitter.to_be_bytes());
        block[16..20].copy_from_slice(&report.last_sender_report.to_be_bytes());
        block[20..24].copy_from_slice(&report.delay_since_sender_report.to_be_bytes());
    }
    packet
}

fn encode_nack(sender_ssrc: u32, media_ssrc: u32, missing: &[u16]) -> Vec<u8> {
    debug_assert!(!missing.is_empty(), "a NACK reports at least one sequence");
    let mut entries = Vec::new();
    let mut index = 0usize;
    while let Some(&pid) = missing.get(index) {
        index = index.saturating_add(1);
        let mut bitmask = 0u16;
        while let Some(&sequence) = missing.get(index) {
            let distance = sequence.wrapping_sub(pid);
            if !(1..=16).contains(&distance) {
                break;
            }
            bitmask |= 1u16 << distance.saturating_sub(1);
            index = index.saturating_add(1);
        }
        entries.push((pid, bitmask));
    }
    let packet_len = 12usize.saturating_add(entries.len().saturating_mul(4));
    debug_assert!(packet_len.is_multiple_of(4));
    let words = packet_len.checked_div(4).unwrap_or(0).saturating_sub(1);
    let mut packet = Vec::with_capacity(packet_len);
    packet.push(0x81);
    packet.push(205);
    packet.extend_from_slice(&u16::try_from(words).unwrap_or(u16::MAX).to_be_bytes());
    packet.extend_from_slice(&sender_ssrc.to_be_bytes());
    packet.extend_from_slice(&media_ssrc.to_be_bytes());
    for (pid, bitmask) in entries {
        packet.extend_from_slice(&pid.to_be_bytes());
        packet.extend_from_slice(&bitmask.to_be_bytes());
    }
    debug_assert_eq!(packet.len(), packet_len);
    packet
}

fn extension_id(section: &NegotiatedMediaSection, matches: impl Fn(&str) -> bool) -> Option<u8> {
    section
        .header_extensions()
        .iter()
        .find(|extension| matches(extension.uri()))
        .map(crate::HeaderExtension::id)
}

fn extension_text(
    entries: &[crate::packet::RtpExtensionEntry],
    bytes: &[u8],
    id: u8,
) -> Option<String> {
    std::str::from_utf8(extension_value(entries, bytes, id)?)
        .ok()
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn extension_value<'a>(
    entries: &[crate::packet::RtpExtensionEntry],
    bytes: &'a [u8],
    id: u8,
) -> Option<&'a [u8]> {
    bytes.get(extension_range(entries, id)?)
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, time::Duration};

    use super::*;
    use crate::{Codec, TransportProtocol};

    fn offer(direction: &str) -> String {
        format!(
            "v=0\r\n\
             o=- 1 2 IN IP4 127.0.0.1\r\n\
             s=-\r\n\
             t=0 0\r\n\
             a=group:BUNDLE 0\r\n\
             a=ice-ufrag:remoteufrag\r\n\
             a=ice-pwd:remotepassword\r\n\
             a=fingerprint:sha-256 01:02:03:04\r\n\
             a=setup:actpass\r\n\
             a=candidate:2 1 UDP 2130706431 127.0.0.1 9001 typ host\r\n\
             m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
             c=IN IP4 0.0.0.0\r\n\
             a=mid:0\r\n\
             a={direction}\r\n\
             a=rtcp-mux\r\n\
             a=rtpmap:96 H264/90000\r\n"
        )
    }

    fn peer(now: Instant, direction: &str) -> (RtcPeer, RtcNegotiation) {
        RtcPeer::accept(
            now,
            7,
            &offer(direction),
            "localufrag".to_owned(),
            "localpassword".to_owned(),
            Box::new(["candidate:1 1 UDP 2130706431 127.0.0.1 9000 typ host".to_owned()]),
        )
        .expect("peer negotiation")
    }

    fn forwarding_peer(now: Instant) -> (RtcPeer, EgressSlot) {
        let offer = "v=0\r\n\
                     o=- 1 2 IN IP4 127.0.0.1\r\n\
                     s=-\r\n\
                     t=0 0\r\n\
                     a=group:BUNDLE out\r\n\
                     a=ice-ufrag:remoteufrag\r\n\
                     a=ice-pwd:remotepassword\r\n\
                     a=fingerprint:sha-256 01:02:03:04\r\n\
                     a=setup:actpass\r\n\
                     a=candidate:2 1 UDP 2130706431 127.0.0.1 9001 typ host\r\n\
                     m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
                     c=IN IP4 0.0.0.0\r\n\
                     a=mid:out\r\n\
                     a=recvonly\r\n\
                     a=rtcp-mux\r\n\
                     a=rtpmap:96 H264/90000\r\n";
        let (peer, negotiation) = RtcPeer::accept(
            now,
            7,
            offer,
            "localufrag".to_owned(),
            "localpassword".to_owned(),
            Box::new(["candidate:1 1 UDP 2130706431 127.0.0.1 9000 typ host".to_owned()]),
        )
        .expect("forwarding peer");
        let slot = negotiation
            .media()
            .iter()
            .find_map(NegotiatedMedia::egress)
            .expect("egress slot");
        (peer, slot)
    }

    fn ambiguous_ingress_peer(now: Instant) -> RtcPeer {
        let offer = "v=0\r\n\
                     o=- 1 2 IN IP4 127.0.0.1\r\n\
                     s=-\r\n\
                     t=0 0\r\n\
                     a=group:BUNDLE 0 1\r\n\
                     a=ice-ufrag:remoteufrag\r\n\
                     a=ice-pwd:remotepassword\r\n\
                     a=fingerprint:sha-256 01:02:03:04\r\n\
                     a=setup:actpass\r\n\
                     a=candidate:2 1 UDP 2130706431 127.0.0.1 9001 typ host\r\n\
                     m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
                     c=IN IP4 0.0.0.0\r\n\
                     a=mid:0\r\n\
                     a=sendonly\r\n\
                     a=rtcp-mux\r\n\
                     a=rtpmap:96 H264/90000\r\n\
                     a=rtcp-fb:96 transport-cc\r\n\
                     a=extmap:1 urn:ietf:params:rtp-hdrext:sdes:mid\r\n\
                     a=extmap:3 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01\r\n\
                     m=video 9 UDP/TLS/RTP/SAVPF 96\r\n\
                     c=IN IP4 0.0.0.0\r\n\
                     a=mid:1\r\n\
                     a=sendonly\r\n\
                     a=rtcp-mux\r\n\
                     a=rtpmap:96 H264/90000\r\n\
                     a=rtcp-fb:96 transport-cc\r\n\
                     a=extmap:1 urn:ietf:params:rtp-hdrext:sdes:mid\r\n\
                     a=extmap:3 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01\r\n";
        RtcPeer::accept(
            now,
            7,
            offer,
            "localufrag".to_owned(),
            "localpassword".to_owned(),
            Box::new(["candidate:1 1 UDP 2130706431 127.0.0.1 9000 typ host".to_owned()]),
        )
        .expect("ambiguous ingress peer")
        .0
    }

    fn authenticated_rtp(
        now: Instant,
        packet_id: u64,
        ssrc: u32,
        sequence: u16,
        extensions: &[(u8, &[u8])],
    ) -> crate::AuthenticatedPacket {
        let mut bytes = vec![0x80, 96];
        bytes.extend_from_slice(&sequence.to_be_bytes());
        bytes.extend_from_slice(&(u32::from(sequence).saturating_mul(3_000)).to_be_bytes());
        bytes.extend_from_slice(&ssrc.to_be_bytes());
        if !extensions.is_empty() {
            bytes[0] |= 0x10;
            let mut encoded = Vec::new();
            for (id, value) in extensions {
                debug_assert!((1..15).contains(id));
                debug_assert!((1..=16).contains(&value.len()));
                encoded.push(
                    (id << 4)
                        | u8::try_from(value.len().saturating_sub(1)).expect("extension length"),
                );
                encoded.extend_from_slice(value);
            }
            while !encoded.len().is_multiple_of(4) {
                encoded.push(0);
            }
            bytes.extend_from_slice(&0xbedeu16.to_be_bytes());
            bytes.extend_from_slice(
                &u16::try_from(encoded.len().saturating_div(4))
                    .expect("extension words")
                    .to_be_bytes(),
            );
            bytes.extend_from_slice(&encoded);
        }
        bytes.extend_from_slice(&[0x65, 0x80]);
        crate::AuthenticatedPacket::for_test(
            bytes,
            PacketProvenance::new(
                now,
                TransportMetadata::new(
                    TransportProtocol::Udp,
                    SocketAddr::from(([127, 0, 0, 1], 9001)),
                    SocketAddr::from(([127, 0, 0, 1], 9000)),
                ),
                PacketId::new(packet_id),
            ),
        )
    }

    fn media_packet(codec: &str, payload: &[u8], extensions: &[(u8, &[u8])]) -> MediaPacket {
        let now = Instant::now();
        let mut bytes = vec![0x80, 96, 0xff, 0xfe, 0xff, 0xff, 0xff, 0xfe, 0, 0, 0, 7];
        if !extensions.is_empty() {
            bytes[0] |= 0x10;
            let mut extension_bytes = Vec::new();
            for (id, value) in extensions {
                assert!((1..15).contains(id));
                assert!((1..=16).contains(&value.len()));
                let encoded_length = value
                    .len()
                    .checked_sub(1)
                    .and_then(|length| u8::try_from(length).ok())
                    .expect("length");
                extension_bytes.push((id << 4) | encoded_length);
                extension_bytes.extend_from_slice(value);
            }
            while !extension_bytes.len().is_multiple_of(4) {
                extension_bytes.push(0);
            }
            bytes.extend_from_slice(&0xbedeu16.to_be_bytes());
            bytes.extend_from_slice(
                &u16::try_from(extension_bytes.len() / 4)
                    .expect("extension words")
                    .to_be_bytes(),
            );
            bytes.extend_from_slice(&extension_bytes);
        }
        bytes.extend_from_slice(payload);
        let provenance = PacketProvenance::new(
            now,
            TransportMetadata::new(
                TransportProtocol::Udp,
                SocketAddr::from(([127, 0, 0, 1], 9001)),
                SocketAddr::from(([127, 0, 0, 1], 9000)),
            ),
            PacketId::new(19),
        );
        let packet = match IngressPacket::new(&bytes, provenance)
            .parse()
            .expect("RTP packet")
        {
            PacketView::Rtp(packet) => packet,
            PacketView::Rtcp(_) => panic!("RTP packet"),
        };
        let payload = packet.payload_range();
        let extension_entries = packet.extension_entries().expect("extensions");
        let negotiated_codec = Codec::new(
            96,
            codec.to_owned(),
            if codec.eq_ignore_ascii_case("opus") {
                48_000
            } else {
                90_000
            },
            codec.eq_ignore_ascii_case("opus").then_some(2),
            None,
            false,
            false,
            false,
            false,
        );
        MediaPacket {
            bytes,
            stream: IngressStream::new(1),
            mid: "0".to_owned(),
            rid: Some("f".to_owned()),
            kind: if codec.eq_ignore_ascii_case("opus") {
                MediaKind::Audio
            } else {
                MediaKind::Video
            },
            codec: NegotiatedCodec::from(&negotiated_codec),
            sequence: ExtendedMediaSequence::new(65_534),
            timestamp: ExtendedRtpTimestamp::new(u64::from(u32::MAX).saturating_sub(1)),
            marker: false,
            payload,
            received_at: now,
            packet_id: 19,
            extension_entries,
            extension_ids: NegotiatedExtensionIds {
                absolute_capture_time: Some(1),
                audio_level: Some(2),
                dependency_descriptor: Some(3),
                video_layers_allocation: Some(5),
            },
            semantics: OnceCell::new(),
            extensions: OnceCell::new(),
            parse_counts: MediaParseCounts::default(),
        }
    }

    #[test]
    fn rtc_peer_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<RtcPeer>();
    }

    #[test]
    fn bound_ssrc_routes_media_when_mid_is_no_longer_repeated() {
        let now = Instant::now();
        let mut peer = ambiguous_ingress_peer(now);
        let first = authenticated_rtp(now, 1, 44, 10, &[(1, b"0"), (3, &[0, 10])]);
        let Some(RtcEvent::Media(first)) = peer.normalize_authenticated(first) else {
            panic!("MID binds the first packet to an ingress stream")
        };
        let second = authenticated_rtp(now, 2, 44, 11, &[(3, &[0, 11])]);
        let Some(RtcEvent::Media(second)) = peer.normalize_authenticated(second) else {
            panic!("the bound SSRC routes without repeated MID")
        };

        assert_eq!(second.stream(), first.stream());
        assert_eq!(second.sequence().get(), first.sequence().get() + 1);
    }

    #[test]
    fn ssrc_zero_transport_feedback_is_observed_before_media_demux() {
        let now = Instant::now();
        let mut peer = ambiguous_ingress_peer(now);
        let probe = authenticated_rtp(now, 1, 0, 10, &[(3, &[0x12, 0x34])]);

        assert!(peer.normalize_authenticated(probe).is_none());
        assert!(peer.receiver_reports.is_empty());
        let feedback = peer
            .connection
            .transport_feedback_for_test(now + Duration::from_millis(100))
            .expect("SSRC 0 probe produces transport feedback");
        let packet = IngressPacket::new(
            &feedback,
            PacketProvenance::new(
                now,
                TransportMetadata::new(
                    TransportProtocol::Udp,
                    SocketAddr::from(([127, 0, 0, 1], 9001)),
                    SocketAddr::from(([127, 0, 0, 1], 9000)),
                ),
                PacketId::new(2),
            ),
        )
        .parse()
        .expect("transport feedback parses");
        let PacketView::Rtcp(packet) = packet else {
            panic!("transport feedback is RTCP")
        };
        let reports = crate::gcc::parse_twcc(&packet).expect("transport feedback decodes");

        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].statuses().len(), 1);
        assert_eq!(reports[0].statuses()[0].sequence(), 0x1234);
        assert!(reports[0].statuses()[0].received_at().is_some());
    }

    #[test]
    fn negotiation_exposes_stable_opaque_media_facts() {
        let now = Instant::now();
        let (_, first) = peer(now, "sendonly");
        let (_, second) = peer(now, "sendonly");

        assert_eq!(first.media(), second.media());
        assert_eq!(first.media().len(), 1);
        assert!(first.media()[0].ingress().is_some());
        assert!(first.media()[0].egress().is_none());
        assert_eq!(first.media()[0].mid(), "0");
        assert_eq!(first.media()[0].codecs()[0].name(), "H264");
    }

    #[test]
    fn lifecycle_and_deadlines_are_monotonic() {
        let now = Instant::now();
        let (mut peer, _) = peer(now, "recvonly");

        assert!(matches!(
            peer.poll_event(),
            Some(RtcEvent::ConnectionStateChanged(
                RtcConnectionState::Negotiated
            ))
        ));
        if let Some(deadline) = peer.next_deadline() {
            assert!(deadline >= now);
        }
        peer.handle_timeout(now + Duration::from_millis(1))
            .expect("timeout");
        if let Some(deadline) = peer.next_deadline() {
            assert!(deadline >= now + Duration::from_millis(1));
        }
    }

    #[test]
    fn transport_events_normalize_to_peer_lifecycle_changes() {
        let now = Instant::now();
        let (mut peer, _) = peer(now, "recvonly");
        let _ = peer.poll_event();

        assert!(matches!(
            peer.normalize_transport_event(TransportEvent::IceChecking),
            Some(RtcEvent::ConnectionStateChanged(
                RtcConnectionState::Connecting
            ))
        ));
        assert!(
            peer.normalize_transport_event(TransportEvent::IceConnected)
                .is_none()
        );
        assert!(matches!(
            peer.normalize_transport_event(TransportEvent::DtlsConnected),
            Some(RtcEvent::ConnectionStateChanged(
                RtcConnectionState::Connected
            ))
        ));
        assert!(matches!(
            peer.normalize_transport_event(TransportEvent::DtlsClosed),
            Some(RtcEvent::ConnectionStateChanged(RtcConnectionState::Closed))
        ));
    }

    #[test]
    fn closed_peer_rejects_new_ingress() {
        let now = Instant::now();
        let (mut peer, _) = peer(now, "sendonly");
        assert!(matches!(
            peer.poll_event(),
            Some(RtcEvent::ConnectionStateChanged(
                RtcConnectionState::Negotiated
            ))
        ));
        peer.close(now);
        let datagram = IngressDatagram::new(
            DatagramProtocol::Udp,
            SocketAddr::from(([127, 0, 0, 1], 9001)),
            SocketAddr::from(([127, 0, 0, 1], 9000)),
            vec![1],
        );

        assert_eq!(
            peer.handle_datagram(now, datagram),
            Err(RtcPeerError::Closed)
        );
        assert_eq!(peer.next_deadline(), None);
        assert!(matches!(
            peer.poll_event(),
            Some(RtcEvent::ConnectionStateChanged(
                RtcConnectionState::Draining
            ))
        ));
        assert!(matches!(
            peer.poll_event(),
            Some(RtcEvent::ConnectionStateChanged(RtcConnectionState::Closed))
        ));
    }

    #[test]
    fn facade_method_types_do_not_expose_protocol_engines() {
        let _: fn(&mut RtcPeer, Instant, IngressDatagram) -> Result<(), RtcPeerError> =
            RtcPeer::handle_datagram;
        let _: fn(&mut RtcPeer) -> Option<RtcEvent> = RtcPeer::poll_event;
        let _: fn(&mut RtcPeer, Instant) -> Option<Transmit> = RtcPeer::poll_transmit;
        let _: fn(
            &mut RtcPeer,
            DepartureReceipt,
            Instant,
        ) -> Result<Option<ForwardingLatency>, RtcPeerError> = RtcPeer::confirm_departure;
    }

    #[test]
    fn media_components_parse_once_in_any_access_order() {
        let stap = [24, 0, 2, 0x67, 0, 0, 2, 0x68, 0, 0, 2, 0x65, 0];
        let capture = [1, 2, 3, 4, 5, 6, 7, 8];
        let audio = [42];
        let dependency = [9, 10, 11];
        let video_layers = [0b0110_0001, 0b0101_0100, 1, 2, 4, 8, 16, 32];
        let packet = media_packet(
            "H264",
            &stap,
            &[
                (1, &capture),
                (2, &audio),
                (3, &dependency),
                (5, &video_layers),
            ],
        );

        let extensions = packet.extensions().expect("extensions");
        assert_eq!(extensions.absolute_capture_time(), Some(capture.as_slice()));
        assert_eq!(extensions.audio_level(), Some(-42));
        assert_eq!(
            extensions.dependency_descriptor(),
            Some(dependency.as_slice())
        );
        let allocation = extensions
            .video_layers_allocation()
            .expect("video layers allocation");
        assert_eq!(allocation.current_stream(), 1);
        assert_eq!(allocation.streams().len(), 3);
        assert_eq!(
            allocation.streams()[0].spatial_layers()[0].cumulative_temporal_kbps(),
            &[1, 2]
        );
        assert_eq!(
            allocation.streams()[1].spatial_layers()[0].cumulative_temporal_kbps(),
            &[4, 8]
        );
        assert_eq!(
            allocation.streams()[2].spatial_layers()[0].cumulative_temporal_kbps(),
            &[16, 32]
        );
        let semantics = *packet.semantics().expect("H.264 semantics");
        assert!(semantics.keyframe());
        assert!(semantics.frame_start());
        let nal = semantics.h264().expect("H.264 metadata");
        assert!(nal.idr());
        assert!(nal.sps());
        assert!(nal.pps());

        let _ = packet.semantics().expect("cached semantics");
        let _ = packet.extensions().expect("cached extensions");
        assert_eq!(packet.parse_count_values(), (1, 1));

        let reverse = media_packet(
            "H264",
            &stap,
            &[
                (1, &capture),
                (2, &audio),
                (3, &dependency),
                (5, &video_layers),
            ],
        );
        let _ = reverse.semantics().expect("semantics first");
        let _ = reverse.extensions().expect("extensions second");
        let _ = reverse.payload();
        let _ = reverse.rid();
        let _ = reverse.semantics().expect("cached semantics");
        assert_eq!(reverse.parse_count_values(), (1, 1));
    }

    #[test]
    fn malformed_and_absent_components_are_cached() {
        let malformed_payload = media_packet("H264", &[], &[]);
        assert_eq!(
            malformed_payload.semantics(),
            Err(MediaPacketError::MalformedH264)
        );
        assert_eq!(
            malformed_payload.semantics(),
            Err(MediaPacketError::MalformedH264)
        );
        assert_eq!(malformed_payload.parse_count_values(), (1, 0));

        let invalid_capture = [1, 2, 3];
        let malformed_extension = media_packet("H264", &[0x61], &[(1, &invalid_capture)]);
        assert_eq!(
            malformed_extension.extensions(),
            Err(MediaPacketError::InvalidAbsoluteCaptureTime)
        );
        assert_eq!(
            malformed_extension.extensions(),
            Err(MediaPacketError::InvalidAbsoluteCaptureTime)
        );
        assert_eq!(malformed_extension.parse_count_values(), (0, 1));

        let invalid_video_layers = [0b0110_0000];
        let malformed_video_layers = media_packet("H264", &[0x61], &[(5, &invalid_video_layers)]);
        assert_eq!(
            malformed_video_layers.extensions(),
            Err(MediaPacketError::InvalidVideoLayersAllocation)
        );
        assert_eq!(
            malformed_video_layers.extensions(),
            Err(MediaPacketError::InvalidVideoLayersAllocation)
        );
        assert_eq!(malformed_video_layers.parse_count_values(), (0, 1));

        let absent = media_packet("H264", &[0x61], &[]);
        assert_eq!(
            absent.extensions(),
            Ok(MediaExtensions {
                absolute_capture_time: None,
                audio_level: None,
                dependency_descriptor: None,
                video_layers_allocation: None,
            })
        );
        assert_eq!(absent.parse_count_values(), (0, 1));

        let continuation = media_packet("H264", &[28, 5, 1], &[]);
        let continuation = *continuation.semantics().expect("FU-A continuation");
        assert!(!continuation.keyframe());
        assert!(!continuation.frame_start());

        let opus = media_packet("opus", &[0x78, 1], &[]);
        let opus = *opus.semantics().expect("Opus semantics");
        assert!(opus.keyframe());
        assert!(opus.frame_start());
        assert_eq!(opus.opus_toc(), Some(0x78));
    }

    #[test]
    fn transit_materialization_copies_storage_once_and_preserves_caches() {
        let packet = media_packet("H264", &[0x65, 1, 2], &[]);
        let source_payload = packet.payload().as_ptr();
        let _ = packet.semantics().expect("semantics");
        let _ = packet.extensions().expect("extensions");

        let transit = TransitMediaPacket::materialize(&packet);

        assert_ne!(source_payload, transit.packet().payload().as_ptr());
        assert_eq!(transit.packet().stream(), packet.stream());
        assert_eq!(transit.packet().sequence(), packet.sequence());
        assert_eq!(transit.packet().timestamp(), packet.timestamp());
        assert_eq!(transit.packet().received_at(), packet.received_at());
        let _ = transit.packet().semantics().expect("preserved semantics");
        let _ = transit.packet().extensions().expect("preserved extensions");
        assert_eq!(transit.packet().parse_count_values(), (0, 0));
    }

    #[test]
    fn sequence_and_timestamp_extension_accept_wrap_and_reordering() {
        let mut sequence = SequenceExtender::default();
        assert_eq!(sequence.extend(65_534), 65_534);
        assert_eq!(sequence.extend(65_535), 65_535);
        assert_eq!(sequence.extend(0), 65_536);
        assert_eq!(sequence.extend(1), 65_537);
        assert_eq!(sequence.extend(65_535), 65_535);

        let mut timestamp = TimestampExtender::default();
        assert_eq!(timestamp.extend(u32::MAX - 1), u64::from(u32::MAX) - 1);
        assert_eq!(timestamp.extend(3), u64::from(u32::MAX) + 4);
        assert_eq!(timestamp.extend(u32::MAX), u64::from(u32::MAX));
    }

    #[test]
    fn ingress_nack_register_tracks_gaps_recovery_and_retry_bound() {
        let mut register = IngressNackRegister::default();
        assert!(register.observe(10));
        assert!(register.observe(13));
        assert_eq!(register.report(), vec![11, 12]);
        assert!(register.observe(12));
        assert!(!register.observe(12));
        for _ in 1..INGRESS_NACK_ATTEMPTS {
            assert_eq!(register.report(), vec![11]);
        }
        assert!(register.report().is_empty());
        assert!(!register.has_pending());
        assert!(register.observe(11));
        assert!(!register.observe(11));
    }

    #[test]
    fn encoded_ingress_nack_round_trips_sequences_across_wrap() {
        let missing = [u16::MAX - 1, u16::MAX, 0, 1, 18];
        let bytes = encode_nack(7, 9, &missing);
        let provenance = PacketProvenance::new(
            Instant::now(),
            TransportMetadata::new(
                TransportProtocol::Udp,
                SocketAddr::from(([127, 0, 0, 1], 9001)),
                SocketAddr::from(([127, 0, 0, 1], 9000)),
            ),
            PacketId::new(20),
        );
        let packet = IngressPacket::new(&bytes, provenance)
            .parse()
            .expect("encoded NACK parses");
        let PacketView::Rtcp(packet) = packet else {
            panic!("encoded feedback is RTCP");
        };
        let nacks = packet.nacks().expect("NACK reports");
        assert_eq!(nacks.len(), 1);
        assert_eq!(nacks[0].media_ssrc(), 9);
        assert_eq!(nacks[0].sequences(), missing);
    }

    #[test]
    fn forwarding_admission_borrows_local_and_transit_storage_for_gapped_rewrites() {
        let now = Instant::now();
        let (mut peer, slot) = forwarding_peer(now);
        let packet = media_packet("H264", &[0x61, 1, 2], &[]);
        let admitted_at = packet.received_at();
        let local_storage = packet.bytes.as_ptr() as usize;

        for sequence in [100, 102, 101] {
            let result = peer.forward(
                admitted_at,
                slot,
                &packet,
                MediaRewrite {
                    sequence: ExtendedMediaSequence::new(sequence),
                    timestamp: ExtendedRtpTimestamp::new(sequence.saturating_mul(3_000)),
                    marker: true,
                    dependency: None,
                },
            );
            assert_eq!(result, Ok(()));
            assert_eq!(peer.last_forward_storage, Some(local_storage));
        }

        let transit = TransitMediaPacket::materialize(&packet);
        let transit_storage = transit.packet().bytes.as_ptr() as usize;
        assert_ne!(local_storage, transit_storage);
        let result = peer.forward_transit(
            admitted_at,
            slot,
            &transit,
            MediaRewrite {
                sequence: ExtendedMediaSequence::new(103),
                timestamp: ExtendedRtpTimestamp::new(9_000),
                marker: true,
                dependency: None,
            },
        );
        assert_eq!(result, Ok(()));
        assert_eq!(peer.last_forward_storage, Some(transit_storage));

        let audio = media_packet("opus", &[0x78, 1], &[]);
        assert!(matches!(
            peer.validate_forward(slot, &audio),
            Err(RtcPeerError::IncompatibleMedia)
        ));
    }

    #[test]
    fn desired_above_current_schedules_exact_five_second_probe_opportunities() {
        let now = Instant::now();
        let (mut peer, _) = forwarding_peer(now);
        peer.set_current_bitrate(now, 500_000)
            .expect("static source bitrate");
        peer.set_desired_bitrate(now, 2_000_000)
            .expect("desired bitrate");

        assert_eq!(
            peer.next_maintenance_probe,
            Some(now + MAINTENANCE_PROBE_INTERVAL)
        );
        peer.maybe_request_maintenance_probe(now + MAINTENANCE_PROBE_INTERVAL);
        assert_eq!(
            peer.next_maintenance_probe,
            Some(now + MAINTENANCE_PROBE_INTERVAL + MAINTENANCE_PROBE_INTERVAL)
        );
        peer.set_current_bitrate(now + MAINTENANCE_PROBE_INTERVAL, 0)
            .expect("paused source bitrate");
        assert_eq!(
            peer.next_maintenance_probe,
            Some(now + MAINTENANCE_PROBE_INTERVAL + MAINTENANCE_PROBE_INTERVAL)
        );
        peer.set_current_bitrate(now + MAINTENANCE_PROBE_INTERVAL, 2_000_000)
            .expect("current catches desired");
        assert_eq!(peer.next_maintenance_probe, None);
        peer.set_current_bitrate(now + MAINTENANCE_PROBE_INTERVAL, 0)
            .expect("unsubscribed current bitrate");
        peer.set_desired_bitrate(now + MAINTENANCE_PROBE_INTERVAL, 0)
            .expect("unsubscribed desired bitrate");
        assert_eq!(peer.next_maintenance_probe, None);
    }

    #[test]
    fn departure_receipts_have_one_terminal_operation() {
        let now = Instant::now();
        let (mut peer, _) = forwarding_peer(now);
        let confirmed = DepartureReceipt(7);
        peer.pending_departures.insert(
            confirmed,
            PendingDeparture {
                send_id: None,
                congestion_tracked: false,
                lifecycle: None,
            },
        );
        assert_eq!(peer.confirm_departure(confirmed, now), Ok(None));
        assert_eq!(
            peer.confirm_departure(confirmed, now),
            Err(RtcPeerError::UnknownDepartureReceipt)
        );

        let abandoned = DepartureReceipt(8);
        peer.pending_departures.insert(
            abandoned,
            PendingDeparture {
                send_id: None,
                congestion_tracked: false,
                lifecycle: None,
            },
        );
        assert_eq!(peer.abandon_departure(abandoned), Ok(()));
        assert_eq!(
            peer.abandon_departure(abandoned),
            Err(RtcPeerError::UnknownDepartureReceipt)
        );
    }

    #[test]
    fn forwarding_latency_uses_each_packet_lifecycle_boundary() {
        let ingress_at = Instant::now();
        let admitted_at = ingress_at + Duration::from_millis(2);
        let eligible_at = admitted_at + Duration::from_millis(3);
        let departed_at = eligible_at + Duration::from_millis(4);

        assert_eq!(
            forwarding_latency(
                EgressLifecycle {
                    ingress_at,
                    admitted_at,
                    eligible_at,
                },
                departed_at,
            ),
            ForwardingLatency {
                service: Duration::from_millis(2),
                pacing: Duration::from_millis(3),
                egress: Duration::from_millis(4),
                total: Duration::from_millis(9),
            }
        );
    }

    #[test]
    fn receiver_report_encodes_loss_jitter_and_sender_timing() {
        let now = Instant::now();
        let mut register = ReceiverReportRegister::new(10);
        register.observe(10, 0, now, now, 90_000);
        register.observe(12, 6_000, now + Duration::from_millis(70), now, 90_000);
        register.observe_sender_report(0x1234_5678, now + Duration::from_millis(100));

        let report = register.report(9, now + Duration::from_millis(600));
        let bytes = encode_receiver_report(7, &[report]);

        assert_eq!(bytes.len(), 32);
        assert_eq!(bytes[0], 0x81);
        assert_eq!(bytes[1], 201);
        assert_eq!(u16::from_be_bytes([bytes[2], bytes[3]]), 7);
        assert_eq!(&bytes[4..8], &7u32.to_be_bytes());
        assert_eq!(&bytes[8..12], &9u32.to_be_bytes());
        assert_eq!(bytes[12], 85);
        assert_eq!(&bytes[13..16], &[0, 0, 1]);
        assert_eq!(&bytes[16..20], &12u32.to_be_bytes());
        assert_ne!(&bytes[20..24], &[0, 0, 0, 0]);
        assert_eq!(&bytes[24..28], &0x1234_5678u32.to_be_bytes());
        assert_eq!(&bytes[28..32], &32_768u32.to_be_bytes());
    }

    #[test]
    fn receiver_report_counts_reordered_packets_once() {
        let now = Instant::now();
        let mut register = ReceiverReportRegister::new(20);
        register.observe(20, 0, now, now, 90_000);
        register.observe(22, 6_000, now + Duration::from_millis(70), now, 90_000);
        register.observe(21, 3_000, now + Duration::from_millis(40), now, 90_000);
        register.observe(21, 3_000, now + Duration::from_millis(41), now, 90_000);

        let report = register.report(9, now + Duration::from_secs(1));

        assert_eq!(report.cumulative_lost, 0);
        assert_eq!(report.fraction_lost, 0);
        assert_eq!(report.extended_highest_sequence, 22);
    }
}
