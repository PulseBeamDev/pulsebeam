use std::{
    cell::OnceCell,
    ops::Range,
    time::{Instant, SystemTime},
};

use crate::{
    IngressStream, MediaKind, NegotiatedCodec, PacketProvenance, packet::RtpExtensionEntry,
};

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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NegotiatedExtensionIds {
    pub(crate) mid: Option<u8>,
    pub(crate) rid: Option<u8>,
    pub(crate) repaired_rid: Option<u8>,
    pub(crate) absolute_capture_time: Option<u8>,
    pub(crate) audio_level: Option<u8>,
    pub(crate) dependency_descriptor: Option<u8>,
    pub(crate) video_layers_allocation: Option<u8>,
}

impl NegotiatedExtensionIds {
    pub const fn mid(self) -> Option<u8> {
        self.mid
    }
    pub const fn rid(self) -> Option<u8> {
        self.rid
    }
    pub const fn repaired_rid(self) -> Option<u8> {
        self.repaired_rid
    }
    pub const fn absolute_capture_time(self) -> Option<u8> {
        self.absolute_capture_time
    }
    pub const fn audio_level(self) -> Option<u8> {
        self.audio_level
    }
    pub const fn dependency_descriptor(self) -> Option<u8> {
        self.dependency_descriptor
    }
    pub const fn video_layers_allocation(self) -> Option<u8> {
        self.video_layers_allocation
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedStreamDescriptor {
    stream: IngressStream,
    mid: String,
    rid: Option<String>,
    kind: MediaKind,
    codec: NegotiatedCodec,
    extension_ids: NegotiatedExtensionIds,
}
impl EncodedStreamDescriptor {
    pub(crate) fn new(
        stream: IngressStream,
        mid: String,
        rid: Option<String>,
        kind: MediaKind,
        codec: NegotiatedCodec,
        extension_ids: NegotiatedExtensionIds,
    ) -> Self {
        debug_assert!(!mid.is_empty());
        Self {
            stream,
            mid,
            rid,
            kind,
            codec,
            extension_ids,
        }
    }
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

    pub const fn extension_ids(&self) -> NegotiatedExtensionIds {
        self.extension_ids
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DependencyRewrite(Box<[u8]>);
impl DependencyRewrite {
    pub fn new(bytes: Box<[u8]>) -> Self {
        debug_assert!(!bytes.is_empty());
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

#[derive(Debug)]
pub struct MediaPacket {
    bytes: Vec<u8>,
    stream: IngressStream,
    sequence: ExtendedMediaSequence,
    raw_timestamp: u32,
    timestamp: ExtendedRtpTimestamp,
    marker: bool,
    payload: Range<usize>,
    extension_entries: Box<[RtpExtensionEntry]>,
    provenance: PacketProvenance,
    playout_time: SystemTime,
    semantics: OnceCell<Result<MediaSemantics, MediaPacketError>>,
    absolute_capture_time: OnceCell<Result<Option<Range<usize>>, MediaPacketError>>,
    audio_level: OnceCell<Result<Option<i8>, MediaPacketError>>,
    dependency_descriptor: OnceCell<Result<Option<Range<usize>>, MediaPacketError>>,
    video_layers_allocation: OnceCell<Result<Option<VideoLayersAllocation>, MediaPacketError>>,
    #[cfg(test)]
    parse_counts: MediaParseCounts,
}

impl MediaPacket {
    #[allow(
        clippy::too_many_arguments,
        reason = "packet construction receives the validated wire ranges and compact provenance as one boundary"
    )]
    pub(crate) fn new(
        bytes: Vec<u8>,
        stream: IngressStream,
        sequence: ExtendedMediaSequence,
        raw_timestamp: u32,
        timestamp: ExtendedRtpTimestamp,
        marker: bool,
        payload: Range<usize>,
        extension_entries: Box<[RtpExtensionEntry]>,
        provenance: PacketProvenance,
        playout_time: SystemTime,
    ) -> Self {
        debug_assert!(!bytes.is_empty());
        debug_assert!(payload.start <= payload.end && payload.end <= bytes.len());
        for entry in &extension_entries {
            let range = entry.value();
            debug_assert!(range.start <= range.end && range.end <= bytes.len());
        }
        Self {
            bytes,
            stream,
            sequence,
            raw_timestamp,
            timestamp,
            marker,
            payload,
            extension_entries,
            provenance,
            playout_time,
            semantics: OnceCell::new(),
            absolute_capture_time: OnceCell::new(),
            audio_level: OnceCell::new(),
            dependency_descriptor: OnceCell::new(),
            video_layers_allocation: OnceCell::new(),
            #[cfg(test)]
            parse_counts: MediaParseCounts::default(),
        }
    }

    pub const fn stream(&self) -> IngressStream {
        self.stream
    }
    pub const fn sequence(&self) -> ExtendedMediaSequence {
        self.sequence
    }
    pub const fn timestamp(&self) -> ExtendedRtpTimestamp {
        self.timestamp
    }
    pub const fn raw_timestamp(&self) -> u32 {
        self.raw_timestamp
    }
    pub const fn marker(&self) -> bool {
        self.marker
    }
    pub fn payload(&self) -> &[u8] {
        self.bytes_at(self.payload.clone())
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub const fn received_at(&self) -> Instant {
        self.provenance.received_at()
    }
    pub const fn packet_id(&self) -> u64 {
        self.provenance.packet_id().get()
    }
    pub const fn provenance(&self) -> PacketProvenance {
        self.provenance
    }
    pub const fn playout_time(&self) -> SystemTime {
        self.playout_time
    }

    pub fn semantics(
        &self,
        descriptor: &EncodedStreamDescriptor,
    ) -> Result<&MediaSemantics, MediaPacketError> {
        debug_assert_eq!(self.stream, descriptor.stream);
        self.semantics
            .get_or_init(|| {
                #[cfg(test)]
                self.parse_counts
                    .semantics
                    .set(self.parse_counts.semantics.get().saturating_add(1));
                parse_media_semantics(descriptor.codec(), self.payload())
            })
            .as_ref()
            .map_err(Clone::clone)
    }

    pub fn absolute_capture_time(
        &self,
        descriptor: &EncodedStreamDescriptor,
    ) -> Result<Option<&[u8]>, MediaPacketError> {
        debug_assert_eq!(self.stream, descriptor.stream);
        let parsed = self
            .absolute_capture_time
            .get_or_init(|| {
                #[cfg(test)]
                self.parse_counts.absolute_capture_time.set(
                    self.parse_counts
                        .absolute_capture_time
                        .get()
                        .saturating_add(1),
                );
                let range = descriptor
                    .extension_ids
                    .absolute_capture_time
                    .and_then(|id| extension_range(&self.extension_entries, id));
                if range
                    .as_ref()
                    .is_some_and(|range| !matches!(range.len(), 8 | 16))
                {
                    return Err(MediaPacketError::InvalidAbsoluteCaptureTime);
                }
                Ok(range)
            })
            .as_ref()
            .map_err(Clone::clone)?;
        Ok(parsed.clone().map(|range| self.bytes_at(range)))
    }

    pub fn audio_level(
        &self,
        descriptor: &EncodedStreamDescriptor,
    ) -> Result<Option<i8>, MediaPacketError> {
        debug_assert_eq!(self.stream, descriptor.stream);
        self.audio_level_with_extension_id(descriptor.extension_ids.audio_level)
    }

    pub fn audio_level_with_extension_id(
        &self,
        extension_id: Option<u8>,
    ) -> Result<Option<i8>, MediaPacketError> {
        self.audio_level
            .get_or_init(|| {
                #[cfg(test)]
                self.parse_counts
                    .audio_level
                    .set(self.parse_counts.audio_level.get().saturating_add(1));
                let Some(id) = extension_id else {
                    return Ok(None);
                };
                let Some(range) = extension_range(&self.extension_entries, id) else {
                    return Ok(None);
                };
                let value = self
                    .bytes
                    .get(range)
                    .and_then(|value| value.first())
                    .copied()
                    .ok_or(MediaPacketError::InvalidAudioLevel)?;
                Ok(Some(
                    i8::try_from(value & 0x7f)
                        .ok()
                        .and_then(i8::checked_neg)
                        .unwrap_or(0),
                ))
            })
            .as_ref()
            .map_err(Clone::clone)
            .copied()
    }

    pub fn dependency_descriptor(
        &self,
        descriptor: &EncodedStreamDescriptor,
    ) -> Result<Option<&[u8]>, MediaPacketError> {
        debug_assert_eq!(self.stream, descriptor.stream);
        let parsed = self
            .dependency_descriptor
            .get_or_init(|| {
                #[cfg(test)]
                self.parse_counts.dependency_descriptor.set(
                    self.parse_counts
                        .dependency_descriptor
                        .get()
                        .saturating_add(1),
                );
                let range = descriptor
                    .extension_ids
                    .dependency_descriptor
                    .and_then(|id| extension_range(&self.extension_entries, id));
                if range.as_ref().is_some_and(Range::is_empty) {
                    return Err(MediaPacketError::InvalidDependencyDescriptor);
                }
                Ok(range)
            })
            .as_ref()
            .map_err(Clone::clone)?;
        Ok(parsed.clone().map(|range| self.bytes_at(range)))
    }

    pub fn video_layers_allocation(
        &self,
        descriptor: &EncodedStreamDescriptor,
    ) -> Result<Option<&VideoLayersAllocation>, MediaPacketError> {
        debug_assert_eq!(self.stream, descriptor.stream);
        let parsed = self
            .video_layers_allocation
            .get_or_init(|| {
                #[cfg(test)]
                self.parse_counts.video_layers_allocation.set(
                    self.parse_counts
                        .video_layers_allocation
                        .get()
                        .saturating_add(1),
                );
                let Some(id) = descriptor.extension_ids.video_layers_allocation else {
                    return Ok(None);
                };
                let Some(range) = extension_range(&self.extension_entries, id) else {
                    return Ok(None);
                };
                let value = self
                    .bytes
                    .get(range)
                    .and_then(parse_video_layers_allocation)
                    .ok_or(MediaPacketError::InvalidVideoLayersAllocation)?;
                Ok(Some(value))
            })
            .as_ref()
            .map_err(Clone::clone)?;
        Ok(parsed.as_ref())
    }

    fn bytes_at(&self, range: Range<usize>) -> &[u8] {
        let Some(bytes) = self.bytes.get(range) else {
            debug_assert!(false);
            return &[];
        };
        bytes
    }

    fn cached_clone_with_bytes(&self, bytes: Vec<u8>) -> Self {
        debug_assert_eq!(bytes.len(), self.bytes.len());
        let semantics = OnceCell::new();
        if let Some(value) = self.semantics.get() {
            debug_assert!(semantics.set(value.clone()).is_ok());
        }
        let absolute_capture_time = OnceCell::new();
        if let Some(value) = self.absolute_capture_time.get() {
            debug_assert!(absolute_capture_time.set(value.clone()).is_ok());
        }
        let audio_level = OnceCell::new();
        if let Some(value) = self.audio_level.get() {
            debug_assert!(audio_level.set(value.clone()).is_ok());
        }
        let dependency_descriptor = OnceCell::new();
        if let Some(value) = self.dependency_descriptor.get() {
            debug_assert!(dependency_descriptor.set(value.clone()).is_ok());
        }
        let video_layers_allocation = OnceCell::new();
        if let Some(value) = self.video_layers_allocation.get() {
            debug_assert!(video_layers_allocation.set(value.clone()).is_ok());
        }
        Self {
            bytes,
            stream: self.stream,
            sequence: self.sequence,
            raw_timestamp: self.raw_timestamp,
            timestamp: self.timestamp,
            marker: self.marker,
            payload: self.payload.clone(),
            extension_entries: self.extension_entries.clone(),
            provenance: self.provenance,
            playout_time: self.playout_time,
            semantics,
            absolute_capture_time,
            audio_level,
            dependency_descriptor,
            video_layers_allocation,
            #[cfg(test)]
            parse_counts: MediaParseCounts::default(),
        }
    }
}

#[derive(Debug)]
pub struct TransitMediaPacket(MediaPacket);

impl TransitMediaPacket {
    pub fn materialize(packet: &MediaPacket) -> Self {
        let bytes = packet.bytes.clone();
        debug_assert_ne!(bytes.as_ptr(), packet.bytes.as_ptr());
        Self(packet.cached_clone_with_bytes(bytes))
    }

    pub const fn packet(&self) -> &MediaPacket {
        &self.0
    }
}

#[cfg(test)]
#[derive(Debug, Default)]
struct MediaParseCounts {
    semantics: std::cell::Cell<usize>,
    absolute_capture_time: std::cell::Cell<usize>,
    audio_level: std::cell::Cell<usize>,
    dependency_descriptor: std::cell::Cell<usize>,
    video_layers_allocation: std::cell::Cell<usize>,
}

fn parse_media_semantics(
    codec: &NegotiatedCodec,
    payload: &[u8],
) -> Result<MediaSemantics, MediaPacketError> {
    if codec.name().eq_ignore_ascii_case("h264") {
        return parse_h264_semantics(payload);
    }
    if codec.name().eq_ignore_ascii_case("opus") {
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
                let end = offset
                    .checked_add(2)
                    .ok_or(MediaPacketError::MalformedH264)?;
                let length = payload
                    .get(offset..end)
                    .and_then(|value| value.try_into().ok())
                    .map(u16::from_be_bytes)
                    .map(usize::from)
                    .ok_or(MediaPacketError::MalformedH264)?;
                if length == 0 {
                    return Err(MediaPacketError::MalformedH264);
                }
                let nal_end = end
                    .checked_add(length)
                    .ok_or(MediaPacketError::MalformedH264)?;
                let nal_type = payload
                    .get(end..nal_end)
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

fn extension_range(entries: &[RtpExtensionEntry], id: u8) -> Option<Range<usize>> {
    entries
        .iter()
        .find(|entry| entry.id() == id)
        .map(RtpExtensionEntry::value)
}

fn parse_video_layers_allocation(bytes: &[u8]) -> Option<VideoLayersAllocation> {
    let (&first, after_first) = bytes.split_first()?;
    if first == 0 && after_first.is_empty() {
        return Some(VideoLayersAllocation {
            current_stream: 0,
            streams: Vec::new(),
        });
    }
    let current_stream = first >> 6;
    let stream_count = usize::from((first >> 4) & 0x03).checked_add(1)?;
    let shared = first & 0x0f;
    let (active, after_masks) = if shared != 0 {
        (vec![lower_mask(shared); stream_count], after_first)
    } else {
        let (mask, tail) = after_first.split_at_checked(stream_count.div_ceil(2))?;
        (
            mask.iter()
                .flat_map(|byte| [byte >> 4, byte & 0x0f])
                .take(stream_count)
                .map(lower_mask)
                .collect(),
            tail,
        )
    };
    let active_count = active.iter().flatten().filter(|value| **value).count();
    let (count_bytes, mut tail) = after_masks.split_at_checked(active_count.div_ceil(4))?;
    let temporal_counts = count_bytes
        .iter()
        .flat_map(|byte| [byte >> 6, byte >> 4 & 3, byte >> 2 & 3, byte & 3])
        .map(|count| usize::from(count).checked_add(1))
        .take(active_count)
        .collect::<Option<std::collections::VecDeque<_>>>()?;
    let mut temporal_counts = temporal_counts;
    let total_temporal = temporal_counts.iter().sum::<usize>();
    let mut bitrates = std::collections::VecDeque::with_capacity(total_temporal);
    for _ in 0..total_temporal {
        let (value, rest) = parse_leb(tail)?;
        bitrates.push_back(value);
        tail = rest;
    }
    let mut resolutions = Vec::new();
    if !tail.is_empty() {
        for _ in 0..active_count {
            let (value, rest) = tail.split_at_checked(5)?;
            tail = rest;
            let width = u16::from_be_bytes(value.get(..2)?.try_into().ok()?).checked_add(1)?;
            let height = u16::from_be_bytes(value.get(2..4)?.try_into().ok()?).checked_add(1)?;
            let framerate = *value.get(4)?;
            resolutions.push((width, height, framerate));
        }
        if !tail.is_empty() {
            return None;
        }
    }
    let mut resolutions = resolutions.into_iter();
    let streams = active
        .into_iter()
        .map(|layers| -> Option<VideoStreamAllocation> {
            let spatial_layers = layers
                .into_iter()
                .map(|is_active| -> Option<VideoSpatialLayerAllocation> {
                    if !is_active {
                        return Some(VideoSpatialLayerAllocation {
                            cumulative_temporal_kbps: Vec::new(),
                            resolution: None,
                        });
                    }
                    let count = temporal_counts.pop_front()?;
                    let cumulative_temporal_kbps = (0..count)
                        .map(|_| bitrates.pop_front())
                        .collect::<Option<Vec<_>>>()?;
                    Some(VideoSpatialLayerAllocation {
                        cumulative_temporal_kbps,
                        resolution: resolutions.next(),
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            Some(VideoStreamAllocation { spatial_layers })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(VideoLayersAllocation {
        current_stream,
        streams,
    })
}

fn lower_mask(value: u8) -> Vec<bool> {
    let highest = (0usize..4)
        .rposition(|offset| value & (1 << offset) != 0)
        .map_or(0, |offset| offset.saturating_add(1));
    (0..highest)
        .map(|offset| value & (1 << offset) != 0)
        .collect()
}

fn parse_leb(mut bytes: &[u8]) -> Option<(u64, &[u8])> {
    let mut value = 0u64;
    for index in 0usize..9 {
        let byte = *bytes.first()?;
        bytes = bytes.get(1..)?;
        value |= u64::from(byte & 0x7f) << index.saturating_mul(7);
        if byte & 0x80 == 0 || index == 8 {
            return Some((value, bytes));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Codec, PacketId,
        packet::{IngressPacket, PacketView, TransportMetadata, TransportProtocol},
    };
    use std::net::SocketAddr;

    fn packet(
        codec: &str,
        payload: &[u8],
        values: &[(u8, &[u8])],
    ) -> (MediaPacket, EncodedStreamDescriptor) {
        let mut bytes = vec![0x90, 96, 0, 1, 0, 0, 0, 2, 0, 0, 0, 3];
        let mut extensions = Vec::new();
        for (id, value) in values {
            debug_assert!((1..=14).contains(id));
            debug_assert!((1..=16).contains(&value.len()));
            extensions.push((*id << 4) | u8::try_from(value.len().saturating_sub(1)).unwrap());
            extensions.extend_from_slice(value);
        }
        while !extensions.len().is_multiple_of(4) {
            extensions.push(0);
        }
        bytes.extend_from_slice(&0xbedeu16.to_be_bytes());
        bytes.extend_from_slice(&u16::try_from(extensions.len() / 4).unwrap().to_be_bytes());
        bytes.extend_from_slice(&extensions);
        bytes.extend_from_slice(payload);
        let now = Instant::now();
        let provenance = PacketProvenance::new(
            now,
            TransportMetadata::new(
                TransportProtocol::Udp,
                SocketAddr::from(([127, 0, 0, 1], 9001)),
                SocketAddr::from(([127, 0, 0, 1], 9000)),
            ),
            PacketId::new(1),
        );
        let view = IngressPacket::new(&bytes, provenance).parse().unwrap();
        let PacketView::Rtp(view) = view else {
            panic!("RTP")
        };
        let payload_range = view.payload_range();
        let entries = view.extension_entries().unwrap();
        let stream = IngressStream::new(1);
        let codec_value = Codec::new(
            96,
            codec.to_owned(),
            90_000,
            None,
            None,
            false,
            false,
            false,
            false,
        );
        let codec = NegotiatedCodec::from(&codec_value);
        let descriptor = EncodedStreamDescriptor::new(
            stream,
            "0".to_owned(),
            None,
            MediaKind::Video,
            codec,
            NegotiatedExtensionIds {
                mid: None,
                rid: None,
                repaired_rid: None,
                absolute_capture_time: Some(1),
                audio_level: Some(2),
                dependency_descriptor: Some(3),
                video_layers_allocation: Some(5),
            },
        );
        let packet = MediaPacket::new(
            bytes,
            stream,
            ExtendedMediaSequence::new(1),
            2,
            ExtendedRtpTimestamp::new(2),
            true,
            payload_range,
            entries,
            provenance,
            SystemTime::UNIX_EPOCH,
        );
        (packet, descriptor)
    }

    #[test]
    fn semantic_families_are_independent_and_malformed_vla_does_not_poison_audio() {
        let capture = [1, 2, 3, 4, 5, 6, 7, 8];
        let audio = [42];
        let malformed_vla = [0x60];
        let (packet, descriptor) = packet(
            "H264",
            &[0x65, 1],
            &[(1, &capture), (2, &audio), (5, &malformed_vla)],
        );
        assert_eq!(packet.parse_counts.semantics.get(), 0);
        assert_eq!(packet.parse_counts.absolute_capture_time.get(), 0);
        assert_eq!(packet.parse_counts.audio_level.get(), 0);
        assert_eq!(packet.parse_counts.dependency_descriptor.get(), 0);
        assert_eq!(packet.parse_counts.video_layers_allocation.get(), 0);
        assert!(
            packet
                .semantics(&descriptor)
                .expect("H.264 semantics")
                .keyframe()
        );
        assert!(
            packet
                .semantics(&descriptor)
                .expect("cached H.264 semantics")
                .keyframe()
        );
        assert_eq!(packet.parse_counts.semantics.get(), 1);
        assert_eq!(
            packet.absolute_capture_time(&descriptor),
            Ok(Some(capture.as_slice()))
        );
        assert_eq!(
            packet.absolute_capture_time(&descriptor),
            Ok(Some(capture.as_slice()))
        );
        assert_eq!(packet.parse_counts.absolute_capture_time.get(), 1);
        assert_eq!(packet.audio_level(&descriptor), Ok(Some(-42)));
        assert_eq!(packet.parse_counts.audio_level.get(), 1);
        assert_eq!(packet.dependency_descriptor(&descriptor), Ok(None));
        assert_eq!(packet.parse_counts.dependency_descriptor.get(), 1);
        assert_eq!(
            packet.video_layers_allocation(&descriptor),
            Err(MediaPacketError::InvalidVideoLayersAllocation)
        );
        assert_eq!(
            packet.video_layers_allocation(&descriptor),
            Err(MediaPacketError::InvalidVideoLayersAllocation)
        );
        assert_eq!(packet.parse_counts.video_layers_allocation.get(), 1);
        let transit = TransitMediaPacket::materialize(&packet);
        assert_eq!(
            transit.packet().video_layers_allocation(&descriptor),
            Err(MediaPacketError::InvalidVideoLayersAllocation)
        );
        assert_eq!(
            transit.packet().parse_counts.video_layers_allocation.get(),
            0
        );
        assert_eq!(packet.audio_level(&descriptor), Ok(Some(-42)));
        assert_eq!(packet.parse_counts.audio_level.get(), 1);
    }

    #[test]
    fn transit_copies_bytes_once_and_preserves_playout_and_cached_results() {
        let capture = [1, 2, 3, 4, 5, 6, 7, 8];
        let (packet, descriptor) = packet("H264", &[0x65, 1], &[(1, &capture)]);
        let source_bytes = packet.bytes().as_ptr();
        let source_payload = packet.payload().as_ptr();
        assert_eq!(packet.bytes().as_ptr(), source_bytes);
        assert_eq!(packet.payload().as_ptr(), source_payload);
        let _ = packet.semantics(&descriptor).unwrap();
        let _ = packet.absolute_capture_time(&descriptor).unwrap();
        let playout = packet.playout_time();
        let transit = TransitMediaPacket::materialize(&packet);
        assert_ne!(source_bytes, transit.packet().bytes().as_ptr());
        assert_ne!(source_payload, transit.packet().payload().as_ptr());
        assert_eq!(packet.playout_time(), playout);
        assert_eq!(transit.packet().playout_time(), playout);
        assert!(transit.packet().semantics(&descriptor).is_ok());
        assert_eq!(transit.packet().parse_counts.semantics.get(), 0);
        assert_eq!(
            transit.packet().absolute_capture_time(&descriptor),
            Ok(Some(capture.as_slice()))
        );
        assert_eq!(transit.packet().parse_counts.absolute_capture_time.get(), 0);
    }

    #[test]
    fn semantic_cache_is_lazy_for_h264_and_opus_and_caches_errors() {
        let (h264, h264_descriptor) = packet("H264", &[0x65, 1], &[]);
        assert_eq!(h264.parse_counts.semantics.get(), 0);
        assert!(h264.semantics(&h264_descriptor).is_ok());
        assert!(h264.semantics(&h264_descriptor).is_ok());
        assert_eq!(h264.parse_counts.semantics.get(), 1);

        let (opus, opus_descriptor) = packet("opus", &[0x98], &[]);
        assert!(opus.semantics(&opus_descriptor).is_ok());
        assert!(opus.semantics(&opus_descriptor).is_ok());
        assert_eq!(opus.parse_counts.semantics.get(), 1);

        let (malformed, malformed_descriptor) = packet("H264", &[], &[]);
        assert_eq!(
            malformed.semantics(&malformed_descriptor),
            Err(MediaPacketError::MalformedH264)
        );
        assert_eq!(
            malformed.semantics(&malformed_descriptor),
            Err(MediaPacketError::MalformedH264)
        );
        assert_eq!(malformed.parse_counts.semantics.get(), 1);
        let transit = TransitMediaPacket::materialize(&malformed);
        assert_eq!(
            transit.packet().semantics(&malformed_descriptor),
            Err(MediaPacketError::MalformedH264)
        );
        assert_eq!(transit.packet().parse_counts.semantics.get(), 0);
    }

    #[test]
    fn vla_parser_keeps_wire_bit_order_and_rejects_partial_resolutions() {
        let bytes = [0x61, 0x54, 1, 2, 4, 8, 16, 32];
        let parsed = parse_video_layers_allocation(&bytes).expect("valid VLA");
        assert_eq!(parsed.current_stream(), 1);
        assert_eq!(parsed.streams().len(), 3);
        assert!(
            parsed
                .streams()
                .iter()
                .all(|stream| stream.spatial_layers().len() == 1)
        );
        let bitrates = [[1, 2], [4, 8], [16, 32]];
        for (stream, expected) in parsed.streams().iter().zip(bitrates) {
            assert_eq!(
                stream.spatial_layers()[0].cumulative_temporal_kbps(),
                expected
            );
        }

        let with_resolutions = [
            0x61, 0x54, 1, 2, 4, 8, 16, 32, 1, 63, 0, 179, 15, 2, 127, 1, 103, 30, 4, 255, 2, 207,
            60,
        ];
        let parsed = parse_video_layers_allocation(&with_resolutions).expect("VLA resolutions");
        assert_eq!(
            parsed.streams()[2].spatial_layers()[0].resolution(),
            Some((1280, 720, 60))
        );
        assert!(
            parse_video_layers_allocation(&with_resolutions[..with_resolutions.len() - 1])
                .is_none()
        );
    }

    #[test]
    fn vla_parser_preserves_sparse_shared_and_per_stream_masks() {
        let shared = parse_video_layers_allocation(&[0x55, 0, 1, 2, 3, 4]).expect("shared VLA");
        assert_eq!(shared.streams().len(), 2);
        for stream in shared.streams() {
            assert_eq!(stream.spatial_layers().len(), 3);
            assert!(
                stream.spatial_layers()[1]
                    .cumulative_temporal_kbps()
                    .is_empty()
            );
        }
        assert_eq!(
            shared.streams()[0].spatial_layers()[0].cumulative_temporal_kbps(),
            &[1]
        );
        assert_eq!(
            shared.streams()[0].spatial_layers()[2].cumulative_temporal_kbps(),
            &[2]
        );

        let per_stream =
            parse_video_layers_allocation(&[0x50, 0x51, 0, 1, 2, 3]).expect("per-stream VLA");
        assert_eq!(per_stream.streams()[0].spatial_layers().len(), 3);
        assert_eq!(per_stream.streams()[1].spatial_layers().len(), 1);
        assert_eq!(
            per_stream.streams()[0].spatial_layers()[2].cumulative_temporal_kbps(),
            &[2]
        );
    }

    #[test]
    fn leb_parser_accepts_u63_boundary_and_stays_bounded() {
        let max = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f, 9];
        assert_eq!(parse_leb(&max), Some(((1u64 << 63) - 1, &[9][..])));

        let continued = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 9];
        assert_eq!(parse_leb(&continued), Some(((1u64 << 63) - 1, &[9][..])));
        assert_eq!(parse_leb(&[0x80; 8]), None);
    }
}
