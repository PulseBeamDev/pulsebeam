use std::{
    cell::{Cell, OnceCell},
    fmt,
    marker::PhantomData,
    ops::Range,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use crate::{
    ClockError, HeaderExtension, IngressStream, MediaKind, RtpClockMapper,
    packet::{PacketError, RtpPacket},
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MediaPacketClockError {
    Packet(PacketError),
    Clock(ClockError),
}

impl fmt::Display for MediaPacketClockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Packet(error) => error.fmt(f),
            Self::Clock(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for MediaPacketClockError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Packet(error) => Some(error),
            Self::Clock(error) => Some(error),
        }
    }
}

impl From<PacketError> for MediaPacketClockError {
    fn from(error: PacketError) -> Self {
        Self::Packet(error)
    }
}

impl From<ClockError> for MediaPacketClockError {
    fn from(error: ClockError) -> Self {
        Self::Clock(error)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticFamily {
    H264,
    Opus,
    DependencyDescriptor,
    VideoLayerAllocation,
    CaptureTime,
    AudioLevel,
    PlayoutDelay,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum H264PacketShape {
    SingleNal(u8),
    StapA,
    FuA {
        start: bool,
        end: bool,
        nal_type: u8,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct H264Fact {
    shape: H264PacketShape,
    sps: bool,
    pps: bool,
    idr: bool,
}
impl H264Fact {
    pub const fn shape(self) -> H264PacketShape {
        self.shape
    }
    pub const fn contains_sps(self) -> bool {
        self.sps
    }
    pub const fn contains_pps(self) -> bool {
        self.pps
    }
    pub const fn contains_idr(self) -> bool {
        self.idr
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OpusFact {
    toc: u8,
    frame_count: u8,
}
impl OpusFact {
    pub const fn toc(self) -> u8 {
        self.toc
    }
    pub const fn frame_count(self) -> u8 {
        self.frame_count
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DependencyDescriptorFact {
    frame_number: u16,
    start_of_frame: bool,
    end_of_frame: bool,
    template_id: u8,
    structure_present: bool,
    active_decode_targets_present: bool,
    custom_dtis: bool,
    custom_frame_diffs: bool,
    custom_chains: bool,
}
impl DependencyDescriptorFact {
    pub const fn frame_number(self) -> u16 {
        self.frame_number
    }
    pub const fn start_of_frame(self) -> bool {
        self.start_of_frame
    }
    pub const fn end_of_frame(self) -> bool {
        self.end_of_frame
    }
    pub const fn template_id(self) -> u8 {
        self.template_id
    }
    pub const fn structure_present(self) -> bool {
        self.structure_present
    }
    pub const fn active_decode_targets_present(self) -> bool {
        self.active_decode_targets_present
    }
    pub const fn custom_dtis(self) -> bool {
        self.custom_dtis
    }
    pub const fn custom_frame_diffs(self) -> bool {
        self.custom_frame_diffs
    }
    pub const fn custom_chains(self) -> bool {
        self.custom_chains
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VlaSpatialLayerFact {
    temporal_layers: u8,
    bitrates: [u64; 4],
    resolution: Option<(u16, u16, u8)>,
}
impl VlaSpatialLayerFact {
    pub const fn temporal_layers(self) -> u8 {
        self.temporal_layers
    }
    pub fn bitrates(&self) -> &[u64] {
        self.bitrates
            .get(..usize::from(self.temporal_layers))
            .unwrap_or(&[])
    }
    pub const fn resolution(self) -> Option<(u16, u16, u8)> {
        self.resolution
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VlaStreamFact {
    spatial_layers: u8,
    layers: [VlaSpatialLayerFact; 4],
}
impl VlaStreamFact {
    pub fn spatial_layers(&self) -> &[VlaSpatialLayerFact] {
        self.layers
            .get(..usize::from(self.spatial_layers))
            .unwrap_or(&[])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoLayerAllocationFact {
    current_stream: u8,
    streams: u8,
    values: [VlaStreamFact; 4],
}
impl VideoLayerAllocationFact {
    pub const fn current_stream(self) -> u8 {
        self.current_stream
    }
    pub fn streams(&self) -> &[VlaStreamFact] {
        self.values.get(..usize::from(self.streams)).unwrap_or(&[])
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbsCaptureTimeFact {
    seconds: u32,
    fraction: u32,
    estimated_offset: Option<i64>,
}
impl AbsCaptureTimeFact {
    pub const fn seconds(self) -> u32 {
        self.seconds
    }
    pub const fn fraction(self) -> u32 {
        self.fraction
    }
    pub const fn estimated_offset(self) -> Option<i64> {
        self.estimated_offset
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioLevelFact {
    voice: bool,
    level: u8,
}
impl AudioLevelFact {
    pub const fn voice(self) -> bool {
        self.voice
    }
    pub const fn level(self) -> u8 {
        self.level
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlayoutDelayFact {
    minimum_ms: u16,
    maximum_ms: u16,
}
impl PlayoutDelayFact {
    pub const fn minimum_ms(self) -> u16 {
        self.minimum_ms
    }
    pub const fn maximum_ms(self) -> u16 {
        self.maximum_ms
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaPacketDescriptor {
    stream: IngressStream,
    kind: MediaKind,
    payload_type: u8,
    codec: Box<str>,
    clock_rate: u32,
    ssrc: Option<u32>,
    mid: Option<Box<str>>,
    rid: Option<Box<str>>,
    extensions: Box<[HeaderExtension]>,
}

impl MediaPacketDescriptor {
    #[allow(
        clippy::too_many_arguments,
        reason = "descriptor is the immutable negotiated tuple"
    )]
    pub fn new(
        stream: IngressStream,
        kind: MediaKind,
        codec: String,
        payload_type: u8,
        clock_rate: u32,
        ssrc: Option<u32>,
        mid: Option<String>,
        rid: Option<String>,
        extensions: Box<[HeaderExtension]>,
    ) -> Option<Self> {
        if codec.trim().is_empty()
            || payload_type > 127
            || clock_rate == 0
            || mid.as_deref() == Some("")
            || rid.as_deref() == Some("")
            || extensions.iter().enumerate().any(|(index, extension)| {
                extensions
                    .get(..index)
                    .is_some_and(|previous| previous.iter().any(|item| item.id() == extension.id()))
                    || semantic_uri_family(extension.uri()).is_some_and(|family| {
                        extensions.get(..index).is_some_and(|previous| {
                            previous
                                .iter()
                                .filter_map(|item| semantic_uri_family(item.uri()))
                                .any(|other| other == family)
                        })
                    })
            })
        {
            return None;
        }
        Some(Self {
            stream,
            kind,
            payload_type,
            codec: codec.into(),
            clock_rate,
            ssrc,
            mid: mid.map(Into::into),
            rid: rid.map(Into::into),
            extensions,
        })
    }
    pub const fn stream(&self) -> IngressStream {
        self.stream
    }
    pub const fn kind(&self) -> MediaKind {
        self.kind
    }
    pub const fn payload_type(&self) -> u8 {
        self.payload_type
    }
    pub fn codec(&self) -> &str {
        &self.codec
    }
    pub const fn clock_rate(&self) -> u32 {
        self.clock_rate
    }
    pub const fn ssrc(&self) -> Option<u32> {
        self.ssrc
    }
    pub fn mid(&self) -> Option<&str> {
        self.mid.as_deref()
    }
    pub fn rid(&self) -> Option<&str> {
        self.rid.as_deref()
    }
    pub fn extensions(&self) -> &[HeaderExtension] {
        &self.extensions
    }

    fn extension_id(&self, family: SemanticFamily) -> Option<u8> {
        let matches_uri: fn(&str) -> bool = match family {
            SemanticFamily::DependencyDescriptor => |uri: &str| {
                matches!(
                    uri,
                    "http://www.webrtc.org/experiments/rtp-hdrext/video-dependency-descriptor"
                        | "urn:ietf:params:rtp-hdrext:video-dependency-descriptor"
                        | "https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension"
                )
            },
            SemanticFamily::VideoLayerAllocation => |uri: &str| {
                uri.starts_with(
                    "http://www.webrtc.org/experiments/rtp-hdrext/video-layers-allocation",
                )
            },
            SemanticFamily::CaptureTime => {
                |uri: &str| uri == "http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time"
            }
            SemanticFamily::AudioLevel => {
                |uri: &str| uri == "urn:ietf:params:rtp-hdrext:ssrc-audio-level"
            }
            SemanticFamily::PlayoutDelay => {
                |uri: &str| uri == "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay"
            }
            SemanticFamily::H264 | SemanticFamily::Opus => return None,
        };
        self.extensions
            .iter()
            .find(|extension| matches_uri(extension.uri()))
            .map(HeaderExtension::id)
    }
}

fn semantic_uri_family(uri: &str) -> Option<SemanticFamily> {
    if uri == "http://www.webrtc.org/experiments/rtp-hdrext/video-dependency-descriptor"
        || uri == "urn:ietf:params:rtp-hdrext:video-dependency-descriptor"
        || uri
            == "https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension"
    {
        Some(SemanticFamily::DependencyDescriptor)
    } else if uri
        .starts_with("http://www.webrtc.org/experiments/rtp-hdrext/video-layers-allocation")
    {
        Some(SemanticFamily::VideoLayerAllocation)
    } else if uri == "http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time" {
        Some(SemanticFamily::CaptureTime)
    } else if uri == "urn:ietf:params:rtp-hdrext:ssrc-audio-level" {
        Some(SemanticFamily::AudioLevel)
    } else if uri == "http://www.webrtc.org/experiments/rtp-hdrext/playout-delay" {
        Some(SemanticFamily::PlayoutDelay)
    } else {
        None
    }
}

#[derive(Debug)]
pub struct MediaPacket {
    bytes: Vec<u8>,
    payload: Range<usize>,
    extension_block: Option<Range<usize>>,
    extension_profile: Option<u16>,
    stream: IngressStream,
    ssrc: u32,
    sequence: u64,
    timestamp: u32,
    marker: bool,
    packet_id: u64,
    playout_time: SystemTime,
    h264_fact: OnceCell<Result<H264Fact, PacketError>>,
    opus_fact: OnceCell<Result<OpusFact, PacketError>>,
    dependency_descriptor: OnceCell<Result<Option<DependencyDescriptorFact>, PacketError>>,
    dependency_descriptor_id: Cell<Option<Option<u8>>>,
    video_layer_allocation: OnceCell<Result<Option<VideoLayerAllocationFact>, PacketError>>,
    video_layer_allocation_id: Cell<Option<Option<u8>>>,
    capture_time: OnceCell<Result<Option<AbsCaptureTimeFact>, PacketError>>,
    capture_time_id: Cell<Option<Option<u8>>>,
    audio_level: OnceCell<Result<Option<AudioLevelFact>, PacketError>>,
    audio_level_id: Cell<Option<Option<u8>>>,
    playout_delay: OnceCell<Result<Option<PlayoutDelayFact>, PacketError>>,
    playout_delay_id: Cell<Option<Option<u8>>>,
    _owner_local: PhantomData<Cell<()>>,
}

const _: fn() = || {
    trait NotSyncProof<A> {
        fn marker() {}
    }
    impl<T: ?Sized> NotSyncProof<()> for T {}
    impl<T: ?Sized + Sync> NotSyncProof<core::convert::Infallible> for T {}
    let _ = <MediaPacket as NotSyncProof<_>>::marker;
};

impl MediaPacket {
    pub fn from_rtp_at(
        bytes: Vec<u8>,
        stream: IngressStream,
        sequence: u64,
        packet_id: u64,
        arrival: Instant,
        mapper: &mut RtpClockMapper,
    ) -> Result<Self, MediaPacketClockError> {
        let (payload, extension_block, extension_profile, ssrc, timestamp, marker) = {
            let parsed = RtpPacket::parse(&bytes)?;
            (
                parsed.payload_range(),
                parsed.extension_range(),
                parsed.extension_profile(),
                parsed.ssrc(),
                parsed.timestamp(),
                parsed.marker(),
            )
        };
        let playout_time = mapper.map_packet(timestamp, arrival)?.playout_time();
        Self::from_rtp_parts(
            bytes,
            stream,
            sequence,
            packet_id,
            playout_time,
            payload,
            extension_block,
            extension_profile,
            ssrc,
            timestamp,
            marker,
        )
        .map_err(Into::into)
    }

    pub fn from_rtp(
        bytes: Vec<u8>,
        stream: IngressStream,
        sequence: u64,
        packet_id: u64,
        playout_time: SystemTime,
    ) -> Result<Self, PacketError> {
        let (payload, extension_block, extension_profile, ssrc, timestamp, marker) = {
            let parsed = RtpPacket::parse(&bytes)?;
            (
                parsed.payload_range(),
                parsed.extension_range(),
                parsed.extension_profile(),
                parsed.ssrc(),
                parsed.timestamp(),
                parsed.marker(),
            )
        };
        Self::from_rtp_parts(
            bytes,
            stream,
            sequence,
            packet_id,
            playout_time,
            payload,
            extension_block,
            extension_profile,
            ssrc,
            timestamp,
            marker,
        )
    }

    #[allow(clippy::too_many_arguments, reason = "metadata is one parser result")]
    fn from_rtp_parts(
        bytes: Vec<u8>,
        stream: IngressStream,
        sequence: u64,
        packet_id: u64,
        playout_time: SystemTime,
        payload: Range<usize>,
        extension_block: Option<Range<usize>>,
        extension_profile: Option<u16>,
        ssrc: u32,
        timestamp: u32,
        marker: bool,
    ) -> Result<Self, PacketError> {
        if playout_time.duration_since(UNIX_EPOCH).is_err() {
            return Err(PacketError::InvalidValue);
        }
        debug_assert!(playout_time >= UNIX_EPOCH);
        let packet = Self {
            bytes,
            payload,
            extension_block,
            extension_profile,
            stream,
            ssrc,
            sequence,
            timestamp,
            marker,
            packet_id,
            playout_time,
            h264_fact: OnceCell::new(),
            opus_fact: OnceCell::new(),
            dependency_descriptor: OnceCell::new(),
            dependency_descriptor_id: Cell::new(None),
            video_layer_allocation: OnceCell::new(),
            video_layer_allocation_id: Cell::new(None),
            capture_time: OnceCell::new(),
            capture_time_id: Cell::new(None),
            audio_level: OnceCell::new(),
            audio_level_id: Cell::new(None),
            playout_delay: OnceCell::new(),
            playout_delay_id: Cell::new(None),
            _owner_local: PhantomData,
        };
        packet.assert_ranges();
        Ok(packet)
    }

    pub const fn stream(&self) -> IngressStream {
        self.stream
    }
    pub const fn ssrc(&self) -> u32 {
        self.ssrc
    }
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }
    pub const fn timestamp(&self) -> u32 {
        self.timestamp
    }
    pub const fn marker(&self) -> bool {
        self.marker
    }
    pub const fn packet_id(&self) -> u64 {
        self.packet_id
    }
    pub const fn playout_time(&self) -> SystemTime {
        self.playout_time
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn payload(&self) -> &[u8] {
        self.bytes.get(self.payload.clone()).unwrap_or_default()
    }
    pub fn extension_data(&self) -> Option<&[u8]> {
        self.extension_block
            .as_ref()
            .and_then(|range| self.bytes.get(range.clone()))
    }
    pub const fn extension_profile(&self) -> Option<u16> {
        self.extension_profile
    }
    pub fn extension(&self, id: u8) -> Option<&[u8]> {
        find_extension(
            &self.bytes,
            self.extension_block.as_ref(),
            self.extension_profile,
            id,
        )
    }

    pub fn semantic_extension(
        &self,
        descriptor: &MediaPacketDescriptor,
        family: SemanticFamily,
    ) -> Result<Option<&[u8]>, PacketError> {
        self.assert_descriptor(descriptor)?;
        if matches!(family, SemanticFamily::H264 | SemanticFamily::Opus) {
            return Err(PacketError::InvalidValue);
        }
        Ok(descriptor.extension_id(family).and_then(|id| {
            find_extension(
                &self.bytes,
                self.extension_block.as_ref(),
                self.extension_profile,
                id,
            )
        }))
    }

    pub fn h264_fact(&self, descriptor: &MediaPacketDescriptor) -> Result<H264Fact, PacketError> {
        self.assert_descriptor(descriptor)?;
        if !descriptor.kind().eq(&MediaKind::Video)
            || descriptor.payload_type() != self.payload_type()
            || !descriptor.codec().eq_ignore_ascii_case("h264")
        {
            return Err(PacketError::InvalidValue);
        }
        self.h264_fact
            .get_or_init(|| parse_h264(self.payload()).map_err(|_| PacketError::MalformedH264))
            .as_ref()
            .map(|value| *value)
            .map_err(Clone::clone)
    }

    pub fn opus_fact(&self, descriptor: &MediaPacketDescriptor) -> Result<OpusFact, PacketError> {
        self.assert_descriptor(descriptor)?;
        if !descriptor.kind().eq(&MediaKind::Audio)
            || descriptor.payload_type() != self.payload_type()
            || !descriptor.codec().eq_ignore_ascii_case("opus")
        {
            return Err(PacketError::InvalidValue);
        }
        self.opus_fact
            .get_or_init(|| parse_opus(self.payload()).map_err(|_| PacketError::MalformedOpus))
            .as_ref()
            .map(|value| *value)
            .map_err(Clone::clone)
    }

    pub fn dependency_descriptor_fact(
        &self,
        descriptor: &MediaPacketDescriptor,
    ) -> Result<Option<DependencyDescriptorFact>, PacketError> {
        self.assert_descriptor(descriptor)?;
        self.extension_fact_for_id(
            descriptor.extension_id(SemanticFamily::DependencyDescriptor),
            &self.dependency_descriptor,
            &self.dependency_descriptor_id,
            |bytes| {
                parse_dependency_descriptor(bytes)
                    .map_err(|_| PacketError::InvalidDependencyDescriptor)
            },
        )
    }
    pub fn video_layer_allocation_fact(
        &self,
        descriptor: &MediaPacketDescriptor,
    ) -> Result<Option<VideoLayerAllocationFact>, PacketError> {
        self.assert_descriptor(descriptor)?;
        self.extension_fact_for_id(
            descriptor.extension_id(SemanticFamily::VideoLayerAllocation),
            &self.video_layer_allocation,
            &self.video_layer_allocation_id,
            |bytes| {
                parse_video_layer_allocation(bytes)
                    .map_err(|_| PacketError::InvalidVideoLayerAllocation)
            },
        )
    }
    pub fn capture_time_fact(
        &self,
        descriptor: &MediaPacketDescriptor,
    ) -> Result<Option<AbsCaptureTimeFact>, PacketError> {
        self.assert_descriptor(descriptor)?;
        self.extension_fact_for_id(
            descriptor.extension_id(SemanticFamily::CaptureTime),
            &self.capture_time,
            &self.capture_time_id,
            |bytes| parse_capture_time(bytes).map_err(|_| PacketError::InvalidCaptureTime),
        )
    }
    pub fn audio_level_fact(
        &self,
        descriptor: &MediaPacketDescriptor,
    ) -> Result<Option<AudioLevelFact>, PacketError> {
        self.assert_descriptor(descriptor)?;
        self.extension_fact_for_id(
            descriptor.extension_id(SemanticFamily::AudioLevel),
            &self.audio_level,
            &self.audio_level_id,
            |bytes| parse_audio_level(bytes).map_err(|_| PacketError::InvalidAudioLevel),
        )
    }
    pub fn playout_delay_fact(
        &self,
        descriptor: &MediaPacketDescriptor,
    ) -> Result<Option<PlayoutDelayFact>, PacketError> {
        self.assert_descriptor(descriptor)?;
        self.extension_fact_for_id(
            descriptor.extension_id(SemanticFamily::PlayoutDelay),
            &self.playout_delay,
            &self.playout_delay_id,
            |bytes| parse_playout_delay(bytes).map_err(|_| PacketError::InvalidPlayoutDelay),
        )
    }

    fn extension_fact_for_id<T: Copy>(
        &self,
        id: Option<u8>,
        cache: &OnceCell<Result<Option<T>, PacketError>>,
        cached_id: &Cell<Option<Option<u8>>>,
        parse: impl FnOnce(&[u8]) -> Result<Option<T>, PacketError>,
    ) -> Result<Option<T>, PacketError> {
        if cached_id.get().is_some_and(|prior| prior != id) {
            return Err(PacketError::InvalidValue);
        }
        if cached_id.get().is_none() {
            cached_id.set(Some(id));
        }
        cache
            .get_or_init(|| {
                match id.and_then(|id| {
                    find_extension_range(
                        &self.bytes,
                        self.extension_block.as_ref(),
                        self.extension_profile,
                        id,
                    )
                }) {
                    None => Ok(None),
                    Some(range) => self
                        .bytes
                        .get(range)
                        .ok_or(PacketError::InvalidLength)
                        .and_then(parse),
                }
            })
            .as_ref()
            .map(|value| *value)
            .map_err(Clone::clone)
    }

    fn assert_descriptor(&self, descriptor: &MediaPacketDescriptor) -> Result<(), PacketError> {
        debug_assert_eq!(self.stream, descriptor.stream());
        debug_assert!(descriptor.ssrc().is_none_or(|ssrc| ssrc == self.ssrc));
        debug_assert_eq!(descriptor.payload_type(), self.payload_type());
        if descriptor.stream() != self.stream
            || descriptor.ssrc().is_some_and(|ssrc| ssrc != self.ssrc)
            || descriptor.payload_type() != self.payload_type()
        {
            return Err(PacketError::InvalidValue);
        }
        Ok(())
    }

    pub fn to_transit(&self) -> OwnedMediaPacket {
        self.assert_ranges();
        OwnedMediaPacket {
            bytes: self.bytes.clone(),
            payload: self.payload.clone(),
            extension_block: self.extension_block.clone(),
            extension_profile: self.extension_profile,
            stream: self.stream,
            ssrc: self.ssrc,
            sequence: self.sequence,
            timestamp: self.timestamp,
            marker: self.marker,
            packet_id: self.packet_id,
            playout_time: self.playout_time,
            h264_fact: self.h264_fact.get().cloned(),
            opus_fact: self.opus_fact.get().cloned(),
            dependency_descriptor: self.dependency_descriptor.get().cloned(),
            dependency_descriptor_id: self.dependency_descriptor_id.get(),
            video_layer_allocation: self.video_layer_allocation.get().cloned(),
            video_layer_allocation_id: self.video_layer_allocation_id.get(),
            capture_time: self.capture_time.get().cloned(),
            capture_time_id: self.capture_time_id.get(),
            audio_level: self.audio_level.get().cloned(),
            audio_level_id: self.audio_level_id.get(),
            playout_delay: self.playout_delay.get().cloned(),
            playout_delay_id: self.playout_delay_id.get(),
        }
    }

    fn payload_type(&self) -> u8 {
        self.bytes.get(1).copied().unwrap_or_default() & 0x7f
    }
    fn assert_ranges(&self) {
        debug_assert!(self.payload.start <= self.payload.end);
        debug_assert!(self.payload.end <= self.bytes.len());
        if let Some(range) = &self.extension_block {
            debug_assert!(range.start <= range.end);
            debug_assert!(range.end <= self.bytes.len());
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct OwnedMediaPacket {
    bytes: Vec<u8>,
    payload: Range<usize>,
    extension_block: Option<Range<usize>>,
    extension_profile: Option<u16>,
    stream: IngressStream,
    ssrc: u32,
    sequence: u64,
    timestamp: u32,
    marker: bool,
    packet_id: u64,
    playout_time: SystemTime,
    h264_fact: Option<Result<H264Fact, PacketError>>,
    opus_fact: Option<Result<OpusFact, PacketError>>,
    dependency_descriptor: Option<Result<Option<DependencyDescriptorFact>, PacketError>>,
    dependency_descriptor_id: Option<Option<u8>>,
    video_layer_allocation: Option<Result<Option<VideoLayerAllocationFact>, PacketError>>,
    video_layer_allocation_id: Option<Option<u8>>,
    capture_time: Option<Result<Option<AbsCaptureTimeFact>, PacketError>>,
    capture_time_id: Option<Option<u8>>,
    audio_level: Option<Result<Option<AudioLevelFact>, PacketError>>,
    audio_level_id: Option<Option<u8>>,
    playout_delay: Option<Result<Option<PlayoutDelayFact>, PacketError>>,
    playout_delay_id: Option<Option<u8>>,
}

impl OwnedMediaPacket {
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn payload(&self) -> &[u8] {
        self.bytes.get(self.payload.clone()).unwrap_or_default()
    }
    pub const fn stream(&self) -> IngressStream {
        self.stream
    }
    pub const fn ssrc(&self) -> u32 {
        self.ssrc
    }
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }
    pub const fn timestamp(&self) -> u32 {
        self.timestamp
    }
    pub const fn marker(&self) -> bool {
        self.marker
    }
    pub const fn packet_id(&self) -> u64 {
        self.packet_id
    }
    pub const fn playout_time(&self) -> SystemTime {
        self.playout_time
    }
    pub fn into_media_packet(self) -> Result<MediaPacket, PacketError> {
        if self.playout_time.duration_since(UNIX_EPOCH).is_err() {
            return Err(PacketError::InvalidValue);
        }
        debug_assert!(self.playout_time >= UNIX_EPOCH);
        let packet = MediaPacket {
            bytes: self.bytes,
            payload: self.payload,
            extension_block: self.extension_block,
            extension_profile: self.extension_profile,
            stream: self.stream,
            ssrc: self.ssrc,
            sequence: self.sequence,
            timestamp: self.timestamp,
            marker: self.marker,
            packet_id: self.packet_id,
            playout_time: self.playout_time,
            h264_fact: self.h264_fact.map_or_else(OnceCell::new, OnceCell::from),
            opus_fact: self.opus_fact.map_or_else(OnceCell::new, OnceCell::from),
            dependency_descriptor: self
                .dependency_descriptor
                .map_or_else(OnceCell::new, OnceCell::from),
            dependency_descriptor_id: Cell::new(self.dependency_descriptor_id),
            video_layer_allocation: self
                .video_layer_allocation
                .map_or_else(OnceCell::new, OnceCell::from),
            video_layer_allocation_id: Cell::new(self.video_layer_allocation_id),
            capture_time: self.capture_time.map_or_else(OnceCell::new, OnceCell::from),
            capture_time_id: Cell::new(self.capture_time_id),
            audio_level: self.audio_level.map_or_else(OnceCell::new, OnceCell::from),
            audio_level_id: Cell::new(self.audio_level_id),
            playout_delay: self
                .playout_delay
                .map_or_else(OnceCell::new, OnceCell::from),
            playout_delay_id: Cell::new(self.playout_delay_id),
            _owner_local: PhantomData,
        };
        packet.assert_ranges();
        (packet.payload.start <= packet.payload.end
            && packet.payload.end <= packet.bytes.len()
            && packet
                .extension_block
                .as_ref()
                .is_none_or(|range| range.start <= range.end && range.end <= packet.bytes.len()))
        .then_some(packet)
        .ok_or(PacketError::InvalidLength)
    }
}

fn find_extension<'a>(
    bytes: &'a [u8],
    block: Option<&Range<usize>>,
    profile: Option<u16>,
    wanted: u8,
) -> Option<&'a [u8]> {
    find_extension_range(bytes, block, profile, wanted).and_then(|range| bytes.get(range))
}

fn find_extension_range(
    bytes: &[u8],
    block: Option<&Range<usize>>,
    profile: Option<u16>,
    wanted: u8,
) -> Option<Range<usize>> {
    let range = block?;
    let mut offset = range.start;
    while offset < range.end {
        let byte = *bytes.get(offset)?;
        offset = offset.checked_add(1)?;
        if profile == Some(0xBEDE) {
            if byte == 0 {
                continue;
            }
            let id = byte >> 4;
            if id == 15 {
                return None;
            }
            let length = usize::from(byte & 0x0f).checked_add(1)?;
            let end = offset.checked_add(length)?;
            if id == wanted {
                return (end <= range.end).then_some(offset..end);
            }
            offset = end;
        } else if profile.is_some_and(|value| value & 0xfff0 == 0x1000) {
            if byte == 0 {
                continue;
            }
            let length = usize::from(*bytes.get(offset)?);
            offset = offset.checked_add(1)?;
            let end = offset.checked_add(length)?;
            if byte == wanted {
                return (end <= range.end).then_some(offset..end);
            }
            offset = end;
        } else {
            return None;
        }
    }
    None
}

#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "mandatory DD length is checked before field access"
)]
fn parse_dependency_descriptor(
    bytes: &[u8],
) -> Result<Option<DependencyDescriptorFact>, PacketError> {
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() > 255 || bytes.len() < 3 {
        return Err(PacketError::InvalidValue);
    }
    let mandatory = bytes[0];
    let mut cursor = DependencyBits { bytes, bit: 24 };
    let (
        structure_present,
        active_decode_targets_present,
        custom_dtis,
        custom_frame_diffs,
        custom_chains,
    ) = if bytes.len() > 3 {
        (
            cursor.read_bit()? != 0,
            cursor.read_bit()? != 0,
            cursor.read_bit()? != 0,
            cursor.read_bit()? != 0,
            cursor.read_bit()? != 0,
        )
    } else {
        (false, false, false, false, false)
    };
    let structure = if structure_present {
        Some(read_dd_structure(&mut cursor)?)
    } else {
        None
    };
    let (_, target_count, chain_count, _) = structure.unwrap_or((0, 0, 0, 0));
    if active_decode_targets_present && structure_present {
        if target_count == 0 {
            return Err(PacketError::InvalidValue);
        }
        cursor.skip(usize::from(target_count))?;
    }
    if structure_present {
        if custom_dtis {
            cursor.skip(usize::from(target_count) * 2)?;
        }
        if custom_frame_diffs {
            loop {
                let size = cursor.read_bits(2)?;
                if size == 0 {
                    break;
                }
                cursor.skip(usize::try_from(size).map_err(|_| PacketError::InvalidValue)? * 4)?;
            }
        }
        if custom_chains {
            cursor.skip(usize::from(chain_count) * 8)?;
        }
        cursor.require_zero_tail()?;
    } else if !active_decode_targets_present
        && !custom_dtis
        && !custom_frame_diffs
        && !custom_chains
        && bytes.len() > 4
    {
        return Err(PacketError::InvalidValue);
    }
    Ok(Some(DependencyDescriptorFact {
        start_of_frame: mandatory & 0x80 != 0,
        end_of_frame: mandatory & 0x40 != 0,
        template_id: mandatory & 0x3f,
        frame_number: u16::from_be_bytes([bytes[1], bytes[2]]),
        structure_present,
        active_decode_targets_present,
        custom_dtis,
        custom_frame_diffs,
        custom_chains,
    }))
}

struct DependencyBits<'a> {
    bytes: &'a [u8],
    bit: usize,
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "bit positions are checked against the bounded extension"
)]
impl<'a> DependencyBits<'a> {
    fn read_bit(&mut self) -> Result<u8, PacketError> {
        let byte = *self
            .bytes
            .get(self.bit / 8)
            .ok_or(PacketError::InvalidValue)?;
        let shift = 7 - self.bit % 8;
        self.bit = self.bit.checked_add(1).ok_or(PacketError::InvalidLength)?;
        Ok((byte >> shift) & 1)
    }
    fn read_bits(&mut self, count: usize) -> Result<u32, PacketError> {
        if count > 32 {
            return Err(PacketError::InvalidLength);
        }
        let mut value = 0u32;
        for _ in 0..count {
            value = (value << 1) | u32::from(self.read_bit()?);
        }
        Ok(value)
    }
    fn skip(&mut self, count: usize) -> Result<(), PacketError> {
        self.bit = self
            .bit
            .checked_add(count)
            .ok_or(PacketError::InvalidLength)?;
        if self.bit > self.bytes.len().saturating_mul(8) {
            return Err(PacketError::InvalidValue);
        }
        Ok(())
    }
    fn read_ns(&mut self, n: usize) -> Result<usize, PacketError> {
        if n <= 1 {
            return Ok(0);
        }
        let floor = usize::BITS - 1 - n.leading_zeros();
        let width = floor + 1;
        let modulus = (1usize << width).saturating_sub(n);
        let value = usize::try_from(self.read_bits((width - 1) as usize)?)
            .map_err(|_| PacketError::InvalidValue)?;
        if value < modulus {
            Ok(value)
        } else {
            let extra = usize::from(self.read_bit()?);
            Ok((value << 1).saturating_sub(modulus).saturating_add(extra))
        }
    }
    fn require_zero_tail(&self) -> Result<(), PacketError> {
        let remaining = self.bytes.len().saturating_mul(8).saturating_sub(self.bit);
        if remaining > 7 {
            return Err(PacketError::InvalidValue);
        }
        for bit in self.bit..self.bytes.len().saturating_mul(8) {
            if self
                .bytes
                .get(bit / 8)
                .is_some_and(|byte| byte & (1 << (7 - bit % 8)) != 0)
            {
                return Err(PacketError::InvalidValue);
            }
        }
        Ok(())
    }
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "the DD structure counts are bounded before their checked bit skips"
)]
fn read_dd_structure(bits: &mut DependencyBits<'_>) -> Result<(usize, u8, u8, u8), PacketError> {
    let _template_id_offset = bits.read_bits(6)?;
    let target_count =
        u8::try_from(bits.read_bits(5)? + 1).map_err(|_| PacketError::InvalidValue)?;
    let mut spatial = 0u8;
    let mut temporal = 0u8;
    let mut templates = 0usize;
    loop {
        templates = templates.checked_add(1).ok_or(PacketError::TooManyItems)?;
        if templates > 64 {
            return Err(PacketError::TooManyItems);
        }
        match bits.read_bits(2)? {
            0 => {}
            1 => {
                temporal = temporal.checked_add(1).ok_or(PacketError::TooManyItems)?;
                if temporal >= 8 {
                    return Err(PacketError::TooManyItems);
                }
            }
            2 => {
                spatial = spatial.checked_add(1).ok_or(PacketError::TooManyItems)?;
                if spatial >= 4 {
                    return Err(PacketError::TooManyItems);
                }
                temporal = 0;
            }
            3 => break,
            _ => return Err(PacketError::InvalidValue),
        }
    }
    for _ in 0..templates {
        bits.skip(usize::from(target_count) * 2)?;
    }
    for _ in 0..templates {
        let mut count = 0;
        while bits.read_bit()? != 0 {
            bits.skip(4)?;
            count += 1;
            if count > 16 {
                return Err(PacketError::TooManyItems);
            }
        }
    }
    let chain_count = u8::try_from(bits.read_ns(usize::from(target_count) + 1)?)
        .map_err(|_| PacketError::InvalidValue)?;
    if chain_count > 0 {
        for _ in 0..target_count {
            bits.read_ns(usize::from(chain_count))?;
        }
        bits.skip(templates * usize::from(chain_count) * 4)?;
    }
    let resolutions = bits.read_bit()? != 0;
    if resolutions {
        for _ in 0..=spatial {
            bits.skip(32)?;
        }
    }
    Ok((templates, target_count, chain_count, spatial + 1))
}

#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation,
    clippy::explicit_iter_loop,
    reason = "all VLA indices are bounded by the fixed four-stream/four-layer wire limits"
)]
fn parse_video_layer_allocation(
    bytes: &[u8],
) -> Result<Option<VideoLayerAllocationFact>, PacketError> {
    if bytes.is_empty() {
        return Ok(None);
    }
    let first = bytes[0];
    if first == 0 && bytes.len() == 1 {
        return Ok(Some(VideoLayerAllocationFact {
            current_stream: 0,
            streams: 0,
            values: [empty_vla_stream(); 4],
        }));
    }
    let current_stream = first >> 6;
    let stream_count = usize::from((first >> 4) & 3) + 1;
    if usize::from(current_stream) >= stream_count {
        return Err(PacketError::InvalidValue);
    }
    let mut masks = [0u8; 4];
    let shared = first & 0x0f;
    let mut offset = 1usize;
    if shared != 0 {
        masks[..stream_count].fill(shared);
    } else {
        let mask_bytes = stream_count.div_ceil(2);
        let end = offset
            .checked_add(mask_bytes)
            .ok_or(PacketError::InvalidLength)?;
        let mask = bytes.get(offset..end).ok_or(PacketError::InvalidLength)?;
        for (index, slot) in masks[..stream_count].iter_mut().enumerate() {
            *slot = if index % 2 == 0 {
                mask[index / 2] >> 4
            } else {
                mask[index / 2] & 0x0f
            };
        }
        offset = end;
    }
    let active_count: usize = masks[..stream_count]
        .iter()
        .map(|mask| mask.count_ones() as usize)
        .sum();
    let count_bytes = active_count.div_ceil(4);
    let end = offset
        .checked_add(count_bytes)
        .ok_or(PacketError::InvalidLength)?;
    let counts = bytes.get(offset..end).ok_or(PacketError::InvalidLength)?;
    offset = end;
    let mut temporal_flat = [0u8; 16];
    let mut active_index = 0usize;
    for mask in masks[..stream_count].iter() {
        for spatial in 0..4 {
            if mask & (1 << spatial) != 0 {
                let count_byte = *counts
                    .get(active_index / 4)
                    .ok_or(PacketError::InvalidLength)?;
                let shift = 6 - (active_index % 4) * 2;
                *temporal_flat
                    .get_mut(active_index)
                    .ok_or(PacketError::TooManyItems)? = ((count_byte >> shift) & 3) + 1;
                active_index += 1;
            }
        }
    }
    let mut result = [empty_vla_stream(); 4];
    let mut bitrate = [[0u64; 4]; 16];
    active_index = 0;
    let mut global_layer_index = 0usize;
    for (stream_index, mask) in masks[..stream_count].iter().enumerate() {
        for spatial in 0..4 {
            if mask & (1 << spatial) != 0 {
                let temporal_count = *temporal_flat
                    .get(active_index)
                    .ok_or(PacketError::TooManyItems)?;
                for layer in 0..usize::from(temporal_count) {
                    let (value, rest) =
                        parse_leb(&bytes[offset..]).ok_or(PacketError::InvalidLength)?;
                    *bitrate
                        .get_mut(active_index)
                        .and_then(|layers| layers.get_mut(layer))
                        .ok_or(PacketError::TooManyItems)? = value;
                    offset = bytes.len() - rest.len();
                }
                active_index += 1;
            }
        }
        result[stream_index].spatial_layers = (0..4)
            .rfind(|spatial| mask & (1 << spatial) != 0)
            .map_or(0, |v| v as u8 + 1);
        for spatial in 0..4 {
            if mask & (1 << spatial) != 0 {
                let layer_value = VlaSpatialLayerFact {
                    temporal_layers: temporal_flat
                        .get(global_layer_index)
                        .copied()
                        .ok_or(PacketError::TooManyItems)?,
                    bitrates: *bitrate
                        .get(global_layer_index)
                        .ok_or(PacketError::TooManyItems)?,
                    resolution: None,
                };
                result
                    .get_mut(stream_index)
                    .ok_or(PacketError::TooManyItems)?
                    .layers
                    .get_mut(spatial)
                    .ok_or(PacketError::TooManyItems)?
                    .clone_from(&layer_value);
                global_layer_index = global_layer_index
                    .checked_add(1)
                    .ok_or(PacketError::TooManyItems)?;
            }
        }
    }
    if offset < bytes.len() {
        let resolution_bytes = active_count
            .checked_mul(5)
            .ok_or(PacketError::InvalidLength)?;
        let end = offset
            .checked_add(resolution_bytes)
            .ok_or(PacketError::InvalidLength)?;
        if end != bytes.len() {
            return Err(PacketError::InvalidLength);
        }
        for stream in &mut result[..stream_count] {
            for layer in &mut stream.layers[..stream.spatial_layers as usize] {
                if layer.temporal_layers == 0 {
                    continue;
                }
                let end = offset.checked_add(5).ok_or(PacketError::InvalidLength)?;
                let resolution = bytes.get(offset..end).ok_or(PacketError::InvalidLength)?;
                let width = vla_dimension(resolution.get(0..2).ok_or(PacketError::InvalidLength)?)?;
                let height =
                    vla_dimension(resolution.get(2..4).ok_or(PacketError::InvalidLength)?)?;
                layer.resolution = Some((
                    width,
                    height,
                    *resolution.get(4).ok_or(PacketError::InvalidLength)?,
                ));
                offset = end;
            }
        }
    }
    debug_assert_eq!(offset, bytes.len());
    Ok(Some(VideoLayerAllocationFact {
        current_stream,
        streams: stream_count as u8,
        values: result,
    }))
}

const fn empty_vla_layer() -> VlaSpatialLayerFact {
    VlaSpatialLayerFact {
        temporal_layers: 0,
        bitrates: [0; 4],
        resolution: None,
    }
}
const fn empty_vla_stream() -> VlaStreamFact {
    VlaStreamFact {
        spatial_layers: 0,
        layers: [empty_vla_layer(); 4],
    }
}

fn vla_dimension(bytes: &[u8]) -> Result<u16, PacketError> {
    let encoded = u16::from_be_bytes(bytes.try_into().map_err(|_| PacketError::InvalidLength)?);
    Ok(encoded.saturating_add(1))
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "the nine-byte LEB bound keeps shifts and offsets finite"
)]
fn parse_leb(bytes: &[u8]) -> Option<(u64, &[u8])> {
    let mut value = 0u64;
    for index in 0..9usize {
        let byte = *bytes.get(index)?;
        if index == 8 {
            if byte & 0x80 != 0 || byte & 0x7e != 0 {
                return None;
            }
            value |= u64::from(byte & 1) << 56;
            return Some((value, bytes.get(index + 1..)?));
        }
        value |= u64::from(byte & 0x7f) << (index * 7);
        if byte & 0x80 == 0 {
            return Some((value, bytes.get(index + 1..)?));
        }
    }
    None
}

fn parse_capture_time(bytes: &[u8]) -> Result<Option<AbsCaptureTimeFact>, PacketError> {
    if !matches!(bytes.len(), 8 | 16) {
        return Err(PacketError::InvalidValue);
    }
    let seconds = u32::from_be_bytes(
        bytes
            .get(0..4)
            .ok_or(PacketError::InvalidLength)?
            .try_into()
            .map_err(|_| PacketError::InvalidLength)?,
    );
    let fraction = u32::from_be_bytes(
        bytes
            .get(4..8)
            .ok_or(PacketError::InvalidLength)?
            .try_into()
            .map_err(|_| PacketError::InvalidLength)?,
    );
    let estimated_offset = if bytes.len() == 16 {
        Some(i64::from_be_bytes(
            bytes
                .get(8..16)
                .ok_or(PacketError::InvalidLength)?
                .try_into()
                .map_err(|_| PacketError::InvalidLength)?,
        ))
    } else {
        None
    };
    Ok(Some(AbsCaptureTimeFact {
        seconds,
        fraction,
        estimated_offset,
    }))
}

#[allow(
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    reason = "H264 offsets are checked before each bounded access"
)]
fn parse_h264(payload: &[u8]) -> Result<H264Fact, PacketError> {
    let first = *payload.first().ok_or(PacketError::InvalidValue)?;
    if first & 0x80 != 0 {
        return Err(PacketError::InvalidValue);
    }
    let mut sps = false;
    let mut pps = false;
    let mut idr = false;
    let shape = match first & 0x1f {
        1..=23 if payload.len() >= 2 => {
            sps = first & 0x1f == 7;
            pps = first & 0x1f == 8;
            idr = first & 0x1f == 5;
            H264PacketShape::SingleNal(first & 0x1f)
        }
        24 => {
            let mut offset = 1usize;
            let mut count = 0usize;
            while offset < payload.len() {
                let end = offset.checked_add(2).ok_or(PacketError::InvalidLength)?;
                let length = usize::from(u16::from_be_bytes(
                    payload
                        .get(offset..end)
                        .and_then(|v| v.try_into().ok())
                        .ok_or(PacketError::InvalidValue)?,
                ));
                if length == 0 {
                    return Err(PacketError::InvalidValue);
                }
                let nal_end = end.checked_add(length).ok_or(PacketError::InvalidLength)?;
                let nal = payload
                    .get(end..nal_end)
                    .ok_or(PacketError::InvalidLength)?;
                if nal[0] & 0x80 != 0 || !(1..=23).contains(&(nal[0] & 0x1f)) {
                    return Err(PacketError::InvalidValue);
                }
                sps |= nal[0] & 0x1f == 7;
                pps |= nal[0] & 0x1f == 8;
                idr |= nal[0] & 0x1f == 5;
                offset = nal_end;
                count += 1;
            }
            if count == 0 {
                return Err(PacketError::InvalidValue);
            }
            H264PacketShape::StapA
        }
        28 if payload.len() >= 2 => {
            let header = *payload.get(1).ok_or(PacketError::InvalidValue)?;
            if payload.len() < 3 || header & 0x1f == 0 || header & 0x1f >= 24 || header & 0x20 != 0
            {
                return Err(PacketError::InvalidValue);
            }
            idr = header & 0x1f == 5;
            sps = header & 0x1f == 7;
            pps = header & 0x1f == 8;
            H264PacketShape::FuA {
                start: header & 0x80 != 0,
                end: header & 0x40 != 0,
                nal_type: header & 0x1f,
            }
        }
        _ => return Err(PacketError::InvalidValue),
    };
    Ok(H264Fact {
        shape,
        sps,
        pps,
        idr,
    })
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "Opus offsets are bounded by packet length checks"
)]
fn parse_opus(payload: &[u8]) -> Result<OpusFact, PacketError> {
    let toc = *payload.first().ok_or(PacketError::InvalidValue)?;
    let frame_count = match toc & 3 {
        0 => 1,
        1 | 2 => 2,
        _ => usize::from(*payload.get(1).ok_or(PacketError::InvalidValue)? & 0x3f),
    };
    if frame_count == 0 || frame_count > 48 {
        return Err(PacketError::InvalidValue);
    }
    let samples = opus_samples_per_frame(toc);
    if samples
        .checked_mul(frame_count)
        .is_none_or(|value| value > 5_760)
    {
        return Err(PacketError::InvalidValue);
    }
    let mut offset = match toc & 3 {
        0..=2 => 1,
        _ => 2,
    };
    match toc & 3 {
        0 => validate_opus_frame(payload.len().saturating_sub(offset))?,
        1 => {
            let data = payload.len().saturating_sub(offset);
            if !data.is_multiple_of(2) {
                return Err(PacketError::InvalidValue);
            }
            validate_opus_frame(data / 2)?;
        }
        2 => {
            let (first_len, next) = opus_frame_length(payload, offset)?;
            offset = next;
            let remaining = payload
                .len()
                .checked_sub(offset)
                .ok_or(PacketError::InvalidValue)?;
            if first_len > remaining || first_len > 1_275 || remaining - first_len > 1_275 {
                return Err(PacketError::InvalidValue);
            }
        }
        _ => {
            let config = *payload.get(1).ok_or(PacketError::InvalidValue)?;
            let mut data_end = payload.len();
            if config & 0x40 != 0 {
                let mut padding = 0usize;
                loop {
                    let value = usize::from(*payload.get(offset).ok_or(PacketError::InvalidValue)?);
                    offset = offset.checked_add(1).ok_or(PacketError::InvalidLength)?;
                    padding = padding
                        .checked_add(if value == 255 { 254 } else { value })
                        .ok_or(PacketError::InvalidLength)?;
                    if value != 255 {
                        break;
                    }
                }
                data_end = data_end
                    .checked_sub(padding)
                    .ok_or(PacketError::InvalidValue)?;
                if data_end < offset {
                    return Err(PacketError::InvalidValue);
                }
            }
            if config & 0x80 != 0 {
                let mut explicit = [0usize; 47];
                let mut explicit_total = 0usize;
                for index in 0..frame_count.saturating_sub(1) {
                    let (length, next) = opus_frame_length(payload, offset)?;
                    if length > 1_275 {
                        return Err(PacketError::InvalidValue);
                    }
                    *explicit.get_mut(index).ok_or(PacketError::InvalidValue)? = length;
                    explicit_total = explicit_total
                        .checked_add(length)
                        .ok_or(PacketError::InvalidLength)?;
                    offset = next;
                }
                let remaining = data_end
                    .checked_sub(offset)
                    .ok_or(PacketError::InvalidValue)?;
                if explicit_total > remaining {
                    return Err(PacketError::InvalidValue);
                }
                validate_opus_frame(remaining - explicit_total)?;
            } else {
                let data = data_end
                    .checked_sub(offset)
                    .ok_or(PacketError::InvalidValue)?;
                if !data.is_multiple_of(frame_count) || data / frame_count > 1_275 {
                    return Err(PacketError::InvalidValue);
                }
            }
        }
    }
    Ok(OpusFact {
        toc,
        frame_count: u8::try_from(frame_count).map_err(|_| PacketError::InvalidValue)?,
    })
}

const fn opus_samples_per_frame(toc: u8) -> usize {
    if toc & 0x80 != 0 {
        (48_000usize << ((toc >> 3) & 3)) / 400
    } else if toc & 0x60 == 0x60 {
        if toc & 0x08 != 0 { 960 } else { 480 }
    } else {
        match (toc >> 3) & 3 {
            3 => 2_880,
            shift => (48_000usize << shift) / 100,
        }
    }
}

const fn validate_opus_frame(length: usize) -> Result<(), PacketError> {
    if length <= 1_275 {
        Ok(())
    } else {
        Err(PacketError::InvalidValue)
    }
}

#[allow(
    clippy::arithmetic_side_effects,
    reason = "length-field offsets are checked by the bounded parser"
)]
fn opus_frame_length(bytes: &[u8], offset: usize) -> Result<(usize, usize), PacketError> {
    let value = *bytes.get(offset).ok_or(PacketError::InvalidLength)?;
    if value < 252 {
        return Ok((usize::from(value), offset + 1));
    }
    let low = usize::from(*bytes.get(offset + 1).ok_or(PacketError::InvalidLength)?);
    Ok((
        low.checked_mul(4)
            .and_then(|v| v.checked_add(usize::from(value)))
            .ok_or(PacketError::InvalidLength)?,
        offset + 2,
    ))
}

fn parse_audio_level(bytes: &[u8]) -> Result<Option<AudioLevelFact>, PacketError> {
    if bytes.len() != 1 {
        return Err(PacketError::InvalidValue);
    }
    Ok(bytes.first().copied().map(|byte| AudioLevelFact {
        voice: byte & 0x80 != 0,
        level: byte & 0x7f,
    }))
}

#[allow(
    clippy::indexing_slicing,
    reason = "the exact three-byte length is checked at entry"
)]
fn parse_playout_delay(bytes: &[u8]) -> Result<Option<PlayoutDelayFact>, PacketError> {
    if bytes.len() != 3 {
        return Err(PacketError::InvalidValue);
    }
    let first = bytes[0];
    let middle = bytes[1];
    let last = bytes[2];
    let minimum = (u16::from(first) << 4) | u16::from(middle >> 4);
    let maximum = (u16::from(middle & 0x0f) << 8) | u16::from(last);
    if minimum > maximum || maximum > 200 {
        return Err(PacketError::InvalidValue);
    }
    Ok(Some(PlayoutDelayFact {
        minimum_ms: minimum.saturating_mul(10),
        maximum_ms: maximum.saturating_mul(10),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::UNIX_EPOCH;

    fn packet(payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![
            0x90, 96, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3, 0xbe, 0xde, 0, 1, 0x10, 0xaa, 0, 0,
        ];
        bytes.extend_from_slice(payload);
        bytes
    }
    fn stream() -> IngressStream {
        IngressStream::new(1).unwrap()
    }

    #[test]
    fn owns_one_decrypted_buffer_and_preserves_identity() {
        let packet = MediaPacket::from_rtp(
            packet(&[1, 2, 3]),
            stream(),
            65_537,
            9,
            UNIX_EPOCH + std::time::Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(packet.payload(), &[1, 2, 3]);
        assert_eq!(packet.sequence(), 65_537);
        assert_eq!(
            packet.playout_time(),
            UNIX_EPOCH + std::time::Duration::from_secs(2)
        );
        assert_eq!(packet.extension(1), Some(&[0xaa][..]));
        let original_pointer = packet.bytes().as_ptr();
        let transit = packet.to_transit();
        assert_ne!(transit.bytes().as_ptr(), original_pointer);
        assert_eq!(transit.bytes(), packet.bytes());
        assert_eq!(transit.payload(), packet.payload());
        let roundtrip = transit.into_media_packet().unwrap();
        assert_eq!(
            roundtrip.payload().as_ptr(),
            roundtrip.bytes().as_ptr().wrapping_add(20)
        );
    }

    #[test]
    fn construction_from_arrival_uses_the_explicit_media_clock() {
        let monotonic = Instant::now();
        let anchor =
            crate::ClockAnchor::new(monotonic, UNIX_EPOCH + std::time::Duration::from_secs(2))
                .unwrap();
        let mut mapper = crate::RtpClockMapper::new(anchor, 3, 90_000).unwrap();
        let packet = MediaPacket::from_rtp_at(
            packet(&[1, 2, 3]),
            stream(),
            1,
            2,
            monotonic + std::time::Duration::from_millis(20),
            &mut mapper,
        )
        .unwrap();
        assert_eq!(
            packet.playout_time(),
            UNIX_EPOCH + std::time::Duration::from_secs(2) + std::time::Duration::from_millis(20)
        );
    }

    #[test]
    fn direct_construction_rejects_pre_unix_playout_time() {
        assert!(matches!(
            MediaPacket::from_rtp(
                packet(&[1]),
                stream(),
                1,
                2,
                UNIX_EPOCH - std::time::Duration::from_nanos(1),
            ),
            Err(PacketError::InvalidValue)
        ));
    }

    #[test]
    fn media_packet_is_send_not_sync() {
        fn assert_send<T: Send>() {}
        assert_send::<MediaPacket>();
    }

    #[test]
    fn h264_facts_cover_stap_and_nonreference_fu() {
        let stap = [24, 0, 2, 0x67, 1, 0, 2, 0x68, 2, 0, 2, 0x65, 3];
        let fact = parse_h264(&stap).unwrap();
        assert!(fact.contains_sps() && fact.contains_pps() && fact.contains_idr());
        let fu = [0x1c, 0x85, 0xaa];
        assert!(parse_h264(&fu).unwrap().contains_idr());
        assert!(parse_h264(&[0x1c, 0x87, 0xaa]).unwrap().contains_sps());
        assert!(parse_h264(&[0x1c, 0x88, 0xaa]).unwrap().contains_pps());
        assert!(parse_h264(&[0x1c, 0xa5, 0xaa]).is_err());
    }

    #[test]
    fn opus_layout_accepts_zero_frames_and_rejects_bounds() {
        assert!(parse_opus(&[0x00]).is_ok());
        assert!(parse_opus(&[0x01]).is_ok());
        assert!(parse_opus(&[0x02, 0]).is_ok());
        assert!(parse_opus(&[0x03, 0x43, 0]).is_ok());
        assert!(parse_opus(&[0x03, 0x83, 1, 2, 0xaa, 0xbb, 0xcc, 0xdd]).is_ok());
        assert!(parse_opus(&[0x03, 0x83, 0, 0, 0xaa]).is_ok());
        let mut padded = vec![0x03, 0x43, 255, 0];
        padded.resize(258, 0);
        assert!(parse_opus(&padded).is_ok());
        let mut boundary_251 = vec![0x02, 251];
        boundary_251.resize(254, 0xaa);
        assert!(parse_opus(&boundary_251).is_ok());
        let mut boundary_252 = vec![0x02, 252, 0];
        boundary_252.resize(256, 0xaa);
        assert!(parse_opus(&boundary_252).is_ok());
        assert!(parse_opus(&[0x1b, 0x03]).is_err());
        let mut oversized = vec![0x00];
        oversized.resize(1_277, 0);
        assert!(parse_opus(&oversized).is_err());
        assert!(parse_opus(&[0x03, 0x83, 2, 0]).is_err());
    }

    #[test]
    fn dependency_descriptor_flags_are_bounded_and_three_byte_safe() {
        let fact = parse_dependency_descriptor(&[0xC1, 0x12, 0x34])
            .unwrap()
            .unwrap();
        assert!(fact.start_of_frame() && fact.end_of_frame());
        assert!(!fact.structure_present());
        assert_eq!(fact.frame_number(), 0x1234);
        let extended = parse_dependency_descriptor(&[0, 0, 0, 0]).unwrap().unwrap();
        assert!(!extended.structure_present());
        assert!(parse_dependency_descriptor(&[0, 0, 0, 0x80]).is_err());
        assert!(parse_dependency_descriptor(&[0; 256]).is_err());
        let mut attached = vec![0, 0, 0];
        let mut bit_position = 24usize;
        let mut write = |value: u32, width: usize| {
            for shift in (0..width).rev() {
                let bit = (value >> shift) & 1;
                if bit_position / 8 == attached.len() {
                    attached.push(0);
                }
                if bit != 0 {
                    attached[bit_position / 8] |= 1 << (7 - bit_position % 8);
                }
                bit_position += 1;
            }
        };
        write(0b10000, 5);
        write(0, 6);
        write(0, 5);
        write(3, 2);
        write(0, 2);
        write(0, 1);
        write(0, 1);
        write(0, 1);
        assert!(
            parse_dependency_descriptor(&attached)
                .unwrap()
                .unwrap()
                .structure_present()
        );
    }

    #[test]
    fn descriptor_is_complete_and_extension_cache_is_id_associated() {
        let capture = "http://www.webrtc.org/experiments/rtp-hdrext/abs-capture-time";
        let extension = HeaderExtension::new(1, capture.to_owned()).unwrap();
        assert!(
            MediaPacketDescriptor::new(
                stream(),
                MediaKind::Video,
                String::new(),
                96,
                90_000,
                Some(3),
                None,
                None,
                Box::new([]),
            )
            .is_none()
        );
        assert!(
            MediaPacketDescriptor::new(
                stream(),
                MediaKind::Video,
                "H264".to_owned(),
                200,
                90_000,
                Some(3),
                None,
                None,
                Box::new([]),
            )
            .is_none()
        );
        let duplicate = vec![
            HeaderExtension::new(1, "urn:test:a".to_owned()).unwrap(),
            HeaderExtension::new(1, "urn:test:b".to_owned()).unwrap(),
        ];
        assert!(
            MediaPacketDescriptor::new(
                stream(),
                MediaKind::Video,
                "H264".to_owned(),
                96,
                90_000,
                Some(3),
                None,
                None,
                duplicate.into_boxed_slice(),
            )
            .is_none()
        );
        let descriptor = MediaPacketDescriptor::new(
            stream(),
            MediaKind::Video,
            "H264".to_owned(),
            96,
            90_000,
            Some(3),
            None,
            None,
            vec![extension].into_boxed_slice(),
        )
        .unwrap();
        let packet = MediaPacket::from_rtp(packet(&[0]), stream(), 1, 1, UNIX_EPOCH).unwrap();
        assert_eq!(
            packet.capture_time_fact(&descriptor),
            Err(PacketError::InvalidCaptureTime)
        );
        let descriptor_other_id = MediaPacketDescriptor::new(
            stream(),
            MediaKind::Video,
            "H264".to_owned(),
            96,
            90_000,
            Some(3),
            None,
            None,
            vec![HeaderExtension::new(2, capture.to_owned()).unwrap()].into_boxed_slice(),
        )
        .unwrap();
        assert_eq!(
            packet.capture_time_fact(&descriptor_other_id),
            Err(PacketError::InvalidValue)
        );
    }

    #[test]
    fn media_extension_facts_and_vla_are_bounded() {
        assert!(parse_capture_time(&[0; 8]).is_ok());
        assert!(parse_capture_time(&[0; 16]).is_ok());
        assert!(parse_capture_time(&[0; 9]).is_err());
        assert!(parse_audio_level(&[0x80]).unwrap().unwrap().voice());
        assert!(parse_audio_level(&[]).is_err());
        assert_eq!(
            parse_playout_delay(&[0, 0, 200])
                .unwrap()
                .unwrap()
                .maximum_ms(),
            2_000
        );
        assert!(parse_playout_delay(&[0, 0, 201]).is_err());

        let single = parse_video_layer_allocation(&[1, 0, 0]).unwrap().unwrap();
        assert_eq!(single.streams().len(), 1);
        assert_eq!(single.streams()[0].spatial_layers().len(), 1);
        assert_eq!(single.streams()[0].spatial_layers()[0].bitrates(), &[0]);
        assert!(parse_video_layer_allocation(&[0x41, 0, 0]).is_err());
        assert_eq!(vla_dimension(&[0xff, 0xff]).unwrap(), u16::MAX);
        let with_max_resolution =
            parse_video_layer_allocation(&[1, 0, 0, 0xff, 0xff, 0xff, 0xff, 30])
                .unwrap()
                .unwrap();
        assert_eq!(
            with_max_resolution.streams()[0].spatial_layers()[0].resolution(),
            Some((u16::MAX, u16::MAX, 30))
        );
        let mut multi = vec![0x10, 0xff, 0, 0];
        multi.extend_from_slice(&[0; 8]);
        let multi = parse_video_layer_allocation(&multi).unwrap().unwrap();
        assert_eq!(multi.streams().len(), 2);
        assert_eq!(multi.streams()[0].spatial_layers().len(), 4);
        assert_eq!(multi.streams()[1].spatial_layers().len(), 4);
        for end in 0..multi_wire_len() {
            let _ = parse_video_layer_allocation(&multi_bytes()[..end]);
        }
    }

    #[test]
    fn transit_preserves_cached_codec_semantics() {
        let descriptor = MediaPacketDescriptor::new(
            stream(),
            MediaKind::Video,
            "H264".to_owned(),
            96,
            90_000,
            Some(3),
            None,
            None,
            Box::new([]),
        )
        .unwrap();
        let success =
            MediaPacket::from_rtp(packet(&[0x65, 0xaa]), stream(), 1, 1, UNIX_EPOCH).unwrap();
        let expected = success.h264_fact(&descriptor).unwrap();
        let success = success.to_transit().into_media_packet().unwrap();
        assert_eq!(success.h264_fact(&descriptor), Ok(expected));

        let failure = MediaPacket::from_rtp(packet(&[0]), stream(), 1, 1, UNIX_EPOCH).unwrap();
        assert_eq!(
            failure.h264_fact(&descriptor),
            Err(PacketError::MalformedH264)
        );
        let failure = failure.to_transit().into_media_packet().unwrap();
        assert_eq!(
            failure.h264_fact(&descriptor),
            Err(PacketError::MalformedH264)
        );
    }

    #[test]
    fn vla_leb128_ninth_byte_is_u64_bounded() {
        let mut maximum = vec![0xff; 8];
        maximum.push(1);
        assert_eq!(parse_leb(&maximum).unwrap().0, (1u64 << 57) - 1);
        let mut payload_overflow = vec![0xff; 8];
        payload_overflow.push(2);
        assert!(parse_leb(&payload_overflow).is_none());
        assert!(parse_leb(&[0xff; 9]).is_none());
    }

    fn multi_wire_len() -> usize {
        12
    }

    fn multi_bytes() -> Vec<u8> {
        let mut bytes = vec![0x10, 0xff, 0, 0];
        bytes.extend_from_slice(&[0; 8]);
        bytes
    }
}
