#![allow(
    clippy::disallowed_types,
    reason = "the v3 media-value contract requires immutable shared packet bytes and metadata"
)]

use std::{cell::Cell, marker::PhantomData, ops::Range, sync::Arc};

use bytes::Bytes;

use crate::{FrameId, GlobalMediaTime, SourceIncarnation};

#[derive(Clone, Debug)]
pub struct MediaPacket {
    bytes: Bytes,
    global_media_at: GlobalMediaTime,
    extensions: Arc<[(Arc<str>, Range<usize>)]>,
    clock_quality: ClockQuality,
    _not_sync: PhantomData<Cell<()>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MediaPacketView<'a> {
    pub ssrc: u32,
    pub sequence_number: u16,
    pub timestamp: u32,
    pub marker: bool,
    pub header_len: usize,
    pub payload: &'a [u8],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClockQuality {
    Provisional,
    Synchronized,
    Unverified,
    Discontinuous,
}

impl MediaPacket {
    #[cfg(test)]
    pub(crate) fn new(
        bytes: Bytes,
        global_media_at: GlobalMediaTime,
        extensions: Arc<[(Arc<str>, Range<usize>)]>,
    ) -> Self {
        Self::with_clock_quality(
            bytes,
            global_media_at,
            extensions,
            ClockQuality::Provisional,
        )
    }

    pub(crate) fn with_clock_quality(
        bytes: Bytes,
        global_media_at: GlobalMediaTime,
        extensions: Arc<[(Arc<str>, Range<usize>)]>,
        clock_quality: ClockQuality,
    ) -> Self {
        Self {
            bytes,
            global_media_at,
            extensions,
            clock_quality,
            _not_sync: PhantomData,
        }
    }

    pub(crate) const fn clock_quality(&self) -> ClockQuality {
        self.clock_quality
    }

    pub const fn bytes(&self) -> &Bytes {
        &self.bytes
    }

    pub const fn global_media_at(&self) -> GlobalMediaTime {
        self.global_media_at
    }

    pub fn view(&self) -> Option<MediaPacketView<'_>> {
        let packet = crate::packet::RtpPacket::parse(&self.bytes).ok()?;
        Some(MediaPacketView {
            ssrc: packet.ssrc(),
            sequence_number: packet.sequence(),
            timestamp: packet.timestamp(),
            marker: packet.marker(),
            header_len: packet.payload_range().start,
            payload: packet.payload(),
        })
    }

    pub fn extension(&self, uri: &str) -> Option<&[u8]> {
        let (_, range) = self
            .extensions
            .iter()
            .find(|(candidate, _)| &**candidate == uri)?;
        self.bytes.get(range.clone())
    }

    pub(crate) fn queued_bytes(&self) -> usize {
        self.bytes.len()
    }

    pub fn materialize(&self) -> Result<crate::MaterializedMediaPacket, crate::MediaTransitError> {
        crate::MaterializedMediaPacket::from_packet(self)
    }

    pub(crate) fn transit_extensions(&self) -> &[(Arc<str>, Range<usize>)] {
        &self.extensions
    }

    pub fn to_transit(&self) -> Self {
        Self {
            bytes: Bytes::copy_from_slice(&self.bytes),
            global_media_at: self.global_media_at,
            clock_quality: self.clock_quality,
            extensions: self
                .extensions
                .iter()
                .map(|(uri, range)| (Arc::from(uri.as_ref()), range.clone()))
                .collect::<Vec<_>>()
                .into(),
            _not_sync: PhantomData,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ForwardedMedia {
    pub source: SourceIncarnation,
    pub packet: MediaPacket,
    pub frame: FrameMetadata,
}

impl ForwardedMedia {
    pub fn to_transit(&self) -> Self {
        Self {
            source: self.source,
            packet: self.packet.to_transit(),
            frame: self.frame.to_transit(),
        }
    }
}

/// Resolved source DD facts, shared locally and copied explicitly for another owner.
/// These contain source activity only; receiver selection belongs to SenderAssignment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceDescriptor {
    pub template_id: u8,
    pub decode_target_indications: Box<[pulsebeam_core::dd::DecodeTargetIndication]>,
    /// One source frame reference per chain. None denotes an independent chain.
    pub chain_dependencies: Box<[Option<FrameId>]>,
    pub structure: Arc<pulsebeam_core::dd::TemplateDependencyStructure>,
    pub active_decode_targets: u32,
    /// Whether this source packet refreshed its template structure on the wire.
    pub attached_structure: bool,
}

impl SourceDescriptor {
    // Reserve a whole structure and the largest legal lists even when locally shared.
    pub(crate) const MEMORY_CHARGE: usize = size_of::<Self>()
        .saturating_add(size_of::<pulsebeam_core::dd::TemplateDependencyStructure>())
        .saturating_add(
            size_of::<pulsebeam_core::dd::DecodeTargetIndication>()
                .saturating_mul(pulsebeam_core::dd::MAX_DECODE_TARGETS),
        )
        .saturating_add(
            size_of::<Option<FrameId>>().saturating_mul(pulsebeam_core::dd::MAX_CHAINS),
        );

    pub fn to_transit(&self) -> Self {
        Self {
            structure: Arc::new(self.structure.as_ref().clone()),
            ..self.clone()
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FrameMetadata {
    pub id: FrameId,
    pub boundary: FrameBoundary,
    pub random_access: bool,
    pub discardable: bool,
    pub dependencies: FrameDependencies,
    pub descriptor: Option<Arc<SourceDescriptor>>,
}

impl FrameMetadata {
    pub fn to_transit(&self) -> Self {
        let dependencies = match &self.dependencies {
            FrameDependencies::Unknown => FrameDependencies::Unknown,
            FrameDependencies::Known(values) => {
                FrameDependencies::Known(Arc::from(values.as_ref()))
            }
        };
        Self {
            id: self.id,
            boundary: self.boundary,
            random_access: self.random_access,
            discardable: self.discardable,
            dependencies,
            descriptor: self
                .descriptor
                .as_ref()
                .map(|value| Arc::new(value.to_transit())),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameBoundary {
    Complete,
    Start,
    Middle,
    End,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FrameDependencies {
    Unknown,
    Known(Arc<[FrameId]>),
}

impl FrameDependencies {
    pub const MAX_DIRECT_DEPENDENCIES: usize = 8;

    pub fn known(dependencies: impl Into<Arc<[FrameId]>>) -> Option<Self> {
        let dependencies = dependencies.into();
        (dependencies.len() <= Self::MAX_DIRECT_DEPENDENCIES).then_some(Self::Known(dependencies))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_view_excludes_padding_and_preserves_header_facts() {
        let bytes = Bytes::from_static(&[
            0xa0, 0xe0, 0x12, 0x34, 0, 0, 0, 42, 0, 0, 0, 7, 10, 20, 0, 2,
        ]);
        let packet = MediaPacket::new(bytes, GlobalMediaTime::from_micros(99), Arc::from([]));
        let view = packet.view().unwrap();
        assert_eq!(view.ssrc, 7);
        assert_eq!(view.sequence_number, 0x1234);
        assert_eq!(view.timestamp, 42);
        assert!(view.marker);
        assert_eq!(view.header_len, 12);
        assert_eq!(view.payload, &[10, 20]);
        assert_eq!(packet.global_media_at(), GlobalMediaTime::from_micros(99));
        assert_eq!(packet.to_transit().view(), Some(view));
    }

    #[test]
    fn transit_deep_copies_packet_owned_state() {
        let packet = MediaPacket::with_clock_quality(
            Bytes::from_static(b"headerextensionpayload"),
            GlobalMediaTime::from_micros(42),
            vec![(Arc::from("urn:example:extension"), 6..15)].into(),
            ClockQuality::Synchronized,
        );

        let transit = packet.to_transit();

        assert_eq!(transit.bytes(), packet.bytes());
        assert_eq!(transit.global_media_at(), packet.global_media_at());
        assert_eq!(transit.clock_quality(), ClockQuality::Synchronized);
        assert_eq!(
            transit.extension("urn:example:extension"),
            packet.extension("urn:example:extension")
        );
        assert_ne!(transit.bytes().as_ptr(), packet.bytes().as_ptr());
        assert!(!Arc::ptr_eq(&transit.extensions, &packet.extensions));
        assert!(!Arc::ptr_eq(
            &transit.extensions[0].0,
            &packet.extensions[0].0
        ));
    }
}
