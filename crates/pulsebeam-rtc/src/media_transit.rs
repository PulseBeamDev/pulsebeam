use std::ops::Range;

use crate::{
    GlobalMediaTime, MediaPacket, PacketOrigin, media_packet::ClockQuality, packet::RtpPacket,
};

const MAGIC: &[u8; 4] = b"PBRP";
const VERSION: u8 = 2;
const HEADER_BYTES: usize = 40;
const MAX_BYTES: usize = 512 * 1024;
const MAX_PACKET_BYTES: usize = u16::MAX as usize;
const MAX_EXTENSIONS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[non_exhaustive]
pub enum MediaTransitError {
    #[error("unsupported media transit version")]
    UnsupportedVersion,
    #[error("media transit exceeds its resource bound")]
    TooLarge,
    #[error("invalid media transit representation")]
    Invalid,
}

#[derive(Clone, Debug)]
pub struct MaterializedMediaPacket {
    wire: Box<[u8]>,
    packet_range: Range<usize>,
    global_media_at: GlobalMediaTime,
    origin: PacketOrigin,
    clock_quality: ClockQuality,
    extensions: Box<[(Box<str>, Range<usize>)]>,
}

impl MaterializedMediaPacket {
    pub fn from_bytes(wire: Box<[u8]>) -> Result<Self, MediaTransitError> {
        if wire.len() > MAX_BYTES {
            return Err(MediaTransitError::TooLarge);
        }
        let mut reader = Reader {
            bytes: &wire,
            offset: 0,
        };
        if reader.take(4)? != MAGIC {
            return Err(MediaTransitError::Invalid);
        }
        if reader.take(1)? != [VERSION] {
            return Err(MediaTransitError::UnsupportedVersion);
        }
        let clock_quality = match reader.take(1)? {
            [0] => ClockQuality::Provisional,
            [1] => ClockQuality::Synchronized,
            [2] => ClockQuality::Unverified,
            [3] => ClockQuality::Discontinuous,
            _ => return Err(MediaTransitError::Invalid),
        };
        let extension_count = usize::from(u16::from_be_bytes(reader.array()?));
        if extension_count > MAX_EXTENSIONS {
            return Err(MediaTransitError::TooLarge);
        }
        let global_media_at = GlobalMediaTime::from_be_bytes(reader.array()?);
        let origin = PacketOrigin {
            observed_at: GlobalMediaTime::from_be_bytes(reader.array()?),
            provenance: u64::from_be_bytes(reader.array()?),
        };
        let packet_len = reader.length()?;
        if packet_len > MAX_PACKET_BYTES {
            return Err(MediaTransitError::TooLarge);
        }
        if reader.take(4)? != [0; 4] {
            return Err(MediaTransitError::Invalid);
        }
        let mut extensions: Vec<(Box<str>, Range<usize>)> = Vec::with_capacity(extension_count);
        for _ in 0..extension_count {
            let uri_len = reader.length()?;
            let start = reader.length()?;
            let end = reader.length()?;
            let uri = std::str::from_utf8(reader.take(uri_len)?)
                .map_err(|_| MediaTransitError::Invalid)?;
            if uri.is_empty()
                || uri.chars().any(|c| c.is_control() || c.is_whitespace())
                || extensions.iter().any(|(other, _)| other.as_ref() == uri)
            {
                return Err(MediaTransitError::Invalid);
            }
            extensions.push((uri.into(), start..end));
        }
        let packet_start = reader.offset;
        let bytes = reader.take(packet_len)?;
        if reader.offset != wire.len() {
            return Err(MediaTransitError::Invalid);
        }
        let packet = RtpPacket::parse(bytes).map_err(|_| MediaTransitError::Invalid)?;
        let ranges = packet
            .extensions()
            .map_err(|_| MediaTransitError::Invalid)?
            .map(|extension| extension.range())
            .collect::<Vec<_>>();
        if extensions.iter().any(|(_, range)| !ranges.contains(range)) {
            return Err(MediaTransitError::Invalid);
        }
        let packet_range = packet_start..reader.offset;
        Ok(Self {
            wire,
            packet_range,
            global_media_at,
            origin,
            clock_quality,
            extensions: extensions.into_boxed_slice(),
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.wire
    }

    pub fn into_bytes(self) -> Box<[u8]> {
        self.wire
    }

    #[allow(
        clippy::disallowed_types,
        reason = "local restoration creates fresh immutable RTC packet state"
    )]
    pub fn into_packet(self) -> Result<MediaPacket, MediaTransitError> {
        use bytes::Bytes;
        use std::sync::Arc;

        // Do not retain the serialized wrapper behind a small canonical packet view.
        let packet = self
            .wire
            .get(self.packet_range)
            .ok_or(MediaTransitError::Invalid)?;
        Ok(MediaPacket::with_clock_quality(
            Bytes::copy_from_slice(packet),
            self.global_media_at,
            self.extensions
                .into_vec()
                .into_iter()
                .map(|(uri, range)| (Arc::from(uri), range))
                .collect::<Vec<_>>()
                .into(),
            self.clock_quality,
            self.origin,
        ))
    }

    pub(crate) fn from_packet(packet: &MediaPacket) -> Result<Self, MediaTransitError> {
        if packet.bytes().len() > MAX_PACKET_BYTES
            || packet.transit_extensions().len() > MAX_EXTENSIONS
        {
            return Err(MediaTransitError::TooLarge);
        }
        let size = packet
            .transit_extensions()
            .iter()
            .try_fold(
                HEADER_BYTES.saturating_add(packet.bytes().len()),
                |size, (uri, _)| size.checked_add(12)?.checked_add(uri.len()),
            )
            .ok_or(MediaTransitError::TooLarge)?;
        if size > MAX_BYTES {
            return Err(MediaTransitError::TooLarge);
        }
        let mut wire = Vec::with_capacity(size);
        wire.extend_from_slice(MAGIC);
        wire.push(VERSION);
        wire.push(match packet.clock_quality() {
            ClockQuality::Provisional => 0,
            ClockQuality::Synchronized => 1,
            ClockQuality::Unverified => 2,
            ClockQuality::Discontinuous => 3,
        });
        wire.extend_from_slice(
            &u16::try_from(packet.transit_extensions().len())
                .map_err(|_| MediaTransitError::TooLarge)?
                .to_be_bytes(),
        );
        wire.extend_from_slice(&packet.global_media_at().to_be_bytes());
        wire.extend_from_slice(&packet.origin().observed_at.to_be_bytes());
        wire.extend_from_slice(&packet.origin().provenance.to_be_bytes());
        write_length(&mut wire, packet.bytes().len())?;
        wire.extend_from_slice(&[0; 4]);
        for (uri, range) in packet.transit_extensions() {
            write_length(&mut wire, uri.len())?;
            write_length(&mut wire, range.start)?;
            write_length(&mut wire, range.end)?;
            wire.extend_from_slice(uri.as_bytes());
        }
        wire.extend_from_slice(packet.bytes());
        Self::from_bytes(wire.into_boxed_slice())
    }
}

fn write_length(wire: &mut Vec<u8>, value: usize) -> Result<(), MediaTransitError> {
    wire.extend_from_slice(
        &u32::try_from(value)
            .map_err(|_| MediaTransitError::TooLarge)?
            .to_be_bytes(),
    );
    Ok(())
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], MediaTransitError> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or(MediaTransitError::Invalid)?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or(MediaTransitError::Invalid)?;
        self.offset = end;
        Ok(bytes)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], MediaTransitError> {
        self.take(N)?
            .try_into()
            .map_err(|_| MediaTransitError::Invalid)
    }

    fn length(&mut self) -> Result<usize, MediaTransitError> {
        usize::try_from(u32::from_be_bytes(self.array()?)).map_err(|_| MediaTransitError::Invalid)
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_types,
    clippy::indexing_slicing,
    reason = "owned canonical RTP and immutable view fixtures"
)]
mod tests {
    use bytes::Bytes;
    use std::sync::Arc;

    use super::*;
    use crate::{ForwardedMedia, FrameBoundary, FrameDependencies, FrameId, FrameMetadata};

    fn packet(time: u64, quality: ClockQuality) -> MediaPacket {
        MediaPacket::with_clock_quality(
            Bytes::from_static(&[
                0x90, 111, 0xff, 0xff, 0, 0, 0, 1, 0, 0, 0, 7, 0xbe, 0xde, 0, 1, 0x11, 1, 2, 0,
                0x80, 0xff, 0, 0xab,
            ]),
            GlobalMediaTime::from_micros(time),
            vec![(Arc::from("urn:test:opaque"), 17..19)].into(),
            quality,
            PacketOrigin {
                observed_at: GlobalMediaTime::from_micros(time.wrapping_add(17)),
                provenance: time ^ 0x1234_9876_4321_abcd,
            },
        )
    }

    #[test]
    fn materialization_preserves_exact_time_bytes_views_and_clock_quality() {
        for time in [0, 1, 0x1234_5678_9abc_def0, u64::MAX] {
            for quality in [
                ClockQuality::Provisional,
                ClockQuality::Synchronized,
                ClockQuality::Unverified,
                ClockQuality::Discontinuous,
            ] {
                let original = packet(time, quality);
                let materialized = original.materialize().expect("materialize");
                let copy = materialized.clone();
                assert_ne!(materialized.as_bytes().as_ptr(), copy.as_bytes().as_ptr());
                assert_ne!(
                    materialized.extensions[0].0.as_ptr(),
                    copy.extensions[0].0.as_ptr()
                );
                let restored = MaterializedMediaPacket::from_bytes(copy.into_bytes())
                    .expect("decode")
                    .into_packet()
                    .expect("restore");
                assert_eq!(restored.bytes(), original.bytes());
                assert_ne!(restored.bytes().as_ptr(), original.bytes().as_ptr());
                assert_eq!(restored.global_media_at(), original.global_media_at());
                assert_eq!(restored.clock_quality(), quality);
                assert_eq!(restored.origin(), original.origin());
                assert_eq!(
                    restored.extension("urn:test:opaque"),
                    Some([1, 2].as_slice())
                );
                assert_eq!(restored.extension("urn:absent"), None);
            }
        }
    }

    #[test]
    fn decoding_rejects_truncation_trailing_bytes_and_invalid_metadata() {
        let valid = packet(42, ClockQuality::Synchronized)
            .materialize()
            .expect("materialize");
        for length in 0..valid.as_bytes().len() {
            assert!(
                MaterializedMediaPacket::from_bytes(valid.as_bytes()[..length].into()).is_err()
            );
        }
        let mut trailing = valid.as_bytes().to_vec();
        trailing.push(0);
        assert!(matches!(
            MaterializedMediaPacket::from_bytes(trailing.into_boxed_slice()),
            Err(MediaTransitError::Invalid)
        ));
        for (offset, value, expected) in [
            (0, 0, MediaTransitError::Invalid),
            (4, 255, MediaTransitError::UnsupportedVersion),
            (5, 255, MediaTransitError::Invalid),
            (HEADER_BYTES - 4, 1, MediaTransitError::Invalid),
            (HEADER_BYTES + 12, b' ', MediaTransitError::Invalid),
            (HEADER_BYTES + 12, 255, MediaTransitError::Invalid),
            (valid.packet_range.start, 0, MediaTransitError::Invalid),
        ] {
            let mut wire = valid.as_bytes().to_vec();
            wire[offset] = value;
            assert!(
                matches!(MaterializedMediaPacket::from_bytes(wire.into_boxed_slice()), Err(error) if error == expected)
            );
        }
        let mut wire = valid.as_bytes().to_vec();
        wire[HEADER_BYTES + 8..HEADER_BYTES + 12].copy_from_slice(&24u32.to_be_bytes());
        assert!(matches!(
            MaterializedMediaPacket::from_bytes(wire.into_boxed_slice()),
            Err(MediaTransitError::Invalid)
        ));
        let mut wire = valid.as_bytes().to_vec();
        wire[HEADER_BYTES..HEADER_BYTES + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(
            MaterializedMediaPacket::from_bytes(wire.into_boxed_slice()),
            Err(MediaTransitError::Invalid)
        ));
    }

    #[test]
    fn transit_enforces_independent_resource_bounds() {
        assert!(matches!(
            MaterializedMediaPacket::from_bytes(vec![0; MAX_BYTES + 1].into_boxed_slice()),
            Err(MediaTransitError::TooLarge)
        ));
        let valid = packet(42, ClockQuality::Provisional)
            .materialize()
            .expect("materialize");
        let mut wire = valid.as_bytes().to_vec();
        wire[6..8].copy_from_slice(&257u16.to_be_bytes());
        assert!(matches!(
            MaterializedMediaPacket::from_bytes(wire.into_boxed_slice()),
            Err(MediaTransitError::TooLarge)
        ));
        let mut wire = valid.as_bytes().to_vec();
        wire[HEADER_BYTES - 8..HEADER_BYTES - 4].copy_from_slice(&65_536u32.to_be_bytes());
        assert!(matches!(
            MaterializedMediaPacket::from_bytes(wire.into_boxed_slice()),
            Err(MediaTransitError::TooLarge)
        ));
        let original = packet(42, ClockQuality::Provisional);
        let invalid = MediaPacket::new(
            Bytes::from(vec![0; MAX_PACKET_BYTES + 1]),
            original.global_media_at(),
            Arc::from([]),
        );
        assert!(matches!(
            invalid.materialize(),
            Err(MediaTransitError::TooLarge)
        ));
        let duplicate = MediaPacket::new(
            original.bytes().clone(),
            original.global_media_at(),
            vec![
                (Arc::from("urn:test:opaque"), 17..19),
                (Arc::from("urn:test:opaque"), 17..19),
            ]
            .into(),
        );
        assert!(matches!(
            duplicate.materialize(),
            Err(MediaTransitError::Invalid)
        ));
    }

    #[test]
    fn forwarded_transit_deep_copies_known_dependencies_and_keeps_unknown_unknown() {
        let frame = FrameMetadata {
            descriptor: None,
            id: FrameId::from_value(9),
            boundary: FrameBoundary::Start,
            random_access: false,
            discardable: true,
            dependencies: FrameDependencies::Known(Arc::from([FrameId::from_value(8)])),
        };
        let original = ForwardedMedia {
            source: pulsebeam_rtc::SourceIncarnation::from_value(1),
            packet: packet(42, ClockQuality::Discontinuous),
            frame,
        };
        let transit = original.to_transit();
        assert_eq!(transit.source, original.source);
        assert_eq!(transit.frame, original.frame);
        assert_eq!(
            transit.packet.global_media_at(),
            original.packet.global_media_at()
        );
        assert_eq!(
            transit.packet.clock_quality(),
            original.packet.clock_quality()
        );
        assert_ne!(
            transit.packet.bytes().as_ptr(),
            original.packet.bytes().as_ptr()
        );
        let (FrameDependencies::Known(a), FrameDependencies::Known(b)) =
            (&original.frame.dependencies, &transit.frame.dependencies)
        else {
            panic!("known dependencies");
        };
        assert!(!Arc::ptr_eq(a, b));
        let unknown = FrameMetadata {
            dependencies: FrameDependencies::Unknown,
            ..original.frame
        };
        assert_eq!(unknown.to_transit(), unknown);
    }

    #[test]
    fn materialized_storage_can_move_between_owners_without_shared_packet_state() {
        fn assert_send<T: Send>() {}
        assert_send::<MaterializedMediaPacket>();
        let packet = packet(42, ClockQuality::Synchronized)
            .materialize()
            .expect("materialize");
        let restored =
            std::thread::spawn(move || packet.into_packet().expect("restore").global_media_at())
                .join()
                .expect("restore on destination owner");
        assert_eq!(restored, GlobalMediaTime::from_micros(42));
    }
}
