//! The fixed 16-byte inter-node Envelope: `version | type | epoch | route |
//! extension`, all big-endian. `route` sits at a fixed offset so the Aya
//! program (and [`peek_route`]) can read the destination shard without
//! parsing the rest of the packet.

use crate::RouteId;

pub const ENVELOPE_LEN: usize = 16;
pub const ROUTE_OFFSET: usize = 4;
pub const ENVELOPE_VERSION: u8 = 1;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeType {
    Media = 0,
    Feedback = 1,
    Telemetry = 2,
}

impl EnvelopeType {
    pub const fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(Self::Media),
            1 => Some(Self::Feedback),
            2 => Some(Self::Telemetry),
            _ => None,
        }
    }

    pub const fn as_u8(self) -> u8 {
        self as u8
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Envelope {
    pub ty: EnvelopeType,
    pub epoch: u16,
    pub route: RouteId,
    pub extension: u64,
}

#[derive(
    zerocopy::FromBytes,
    zerocopy::IntoBytes,
    zerocopy::KnownLayout,
    zerocopy::Immutable,
    zerocopy::Unaligned,
)]
#[repr(C)]
struct EnvelopeWire {
    version: u8,
    ty: u8,
    epoch: zerocopy::big_endian::U16,
    route: zerocopy::big_endian::U32,
    extension: zerocopy::big_endian::U64,
}

const _: () = assert!(core::mem::size_of::<EnvelopeWire>() == ENVELOPE_LEN);
const _: () = assert!(ROUTE_OFFSET == core::mem::offset_of!(EnvelopeWire, route));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvelopeError {
    Truncated { len: usize },
    UnsupportedVersion { ver: u8 },
    UnknownType { ty: u8 },
}

impl Envelope {
    pub fn encode(&self) -> [u8; ENVELOPE_LEN] {
        let wire = EnvelopeWire {
            version: ENVELOPE_VERSION,
            ty: self.ty.as_u8(),
            epoch: self.epoch.into(),
            route: self.route.get().into(),
            extension: self.extension.into(),
        };
        zerocopy::transmute!(wire)
    }

    pub fn decode(buf: &[u8]) -> Result<Self, EnvelopeError> {
        if buf.len() < ENVELOPE_LEN {
            return Err(EnvelopeError::Truncated { len: buf.len() });
        }
        let (wire, _rest) = zerocopy::FromBytes::ref_from_prefix(buf)
            .map_err(|_| EnvelopeError::Truncated { len: buf.len() })?;
        let wire: &EnvelopeWire = wire;
        if wire.version != ENVELOPE_VERSION {
            return Err(EnvelopeError::UnsupportedVersion { ver: wire.version });
        }
        let ty =
            EnvelopeType::from_u8(wire.ty).ok_or(EnvelopeError::UnknownType { ty: wire.ty })?;
        Ok(Self {
            ty,
            epoch: wire.epoch.get(),
            route: RouteId::from_raw(wire.route.get()),
            extension: wire.extension.get(),
        })
    }
}

/// Bounds-checked fixed-offset route read for the steering path. Does not
/// validate version or type — the eBPF program only needs to know which
/// shard socket to hand the datagram to, not whether userspace will
/// ultimately accept it.
pub fn peek_route(buf: &[u8]) -> Option<RouteId> {
    let route_end = ROUTE_OFFSET.checked_add(4)?;
    let bytes: [u8; 4] = buf.get(ROUTE_OFFSET..route_end)?.try_into().ok()?;
    Some(RouteId::from_raw(u32::from_be_bytes(bytes)))
}

pub fn peek_shard(buf: &[u8]) -> Option<u16> {
    peek_route(buf).map(RouteId::shard)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope(ty: EnvelopeType, route: RouteId, epoch: u16, extension: u64) -> Envelope {
        Envelope {
            ty,
            epoch,
            route,
            extension,
        }
    }

    #[test]
    fn encode_is_exactly_16_bytes_for_every_type() {
        for ty in [
            EnvelopeType::Media,
            EnvelopeType::Feedback,
            EnvelopeType::Telemetry,
        ] {
            let env = envelope(
                ty,
                RouteId::from_raw(0x1234_5678),
                42,
                0xdead_beef_cafe_babe,
            );
            let bytes = env.encode();
            assert_eq!(bytes.len(), ENVELOPE_LEN);
            let decoded = Envelope::decode(&bytes).unwrap();
            assert_eq!(decoded, env);
        }
    }

    #[test]
    fn encode_decode_round_trips_max_route_and_epoch() {
        let env = envelope(
            EnvelopeType::Media,
            RouteId::from_raw(u32::MAX),
            u16::MAX,
            u64::MAX,
        );
        let bytes = env.encode();
        assert_eq!(Envelope::decode(&bytes).unwrap(), env);
    }

    #[test]
    fn decode_rejects_truncated_input() {
        let bytes = envelope(EnvelopeType::Media, RouteId::from_raw(1), 1, 1).encode();
        for len in 0..ENVELOPE_LEN {
            let err = Envelope::decode(&bytes[..len]).unwrap_err();
            assert_eq!(err, EnvelopeError::Truncated { len });
        }
    }

    #[test]
    fn decode_rejects_wrong_version() {
        let mut bytes = envelope(EnvelopeType::Media, RouteId::from_raw(1), 1, 1).encode();
        bytes[0] = ENVELOPE_VERSION + 1;
        assert_eq!(
            Envelope::decode(&bytes).unwrap_err(),
            EnvelopeError::UnsupportedVersion {
                ver: ENVELOPE_VERSION + 1
            }
        );
    }

    #[test]
    fn decode_rejects_unknown_type() {
        let mut bytes = envelope(EnvelopeType::Media, RouteId::from_raw(1), 1, 1).encode();
        bytes[1] = 250;
        assert_eq!(
            Envelope::decode(&bytes).unwrap_err(),
            EnvelopeError::UnknownType { ty: 250 }
        );
    }

    #[test]
    fn peek_route_agrees_with_full_decode() {
        for route in [0u32, 1, 0x0FFF_FFFF, u32::MAX] {
            let env = envelope(EnvelopeType::Telemetry, RouteId::from_raw(route), 7, 9);
            let bytes = env.encode();
            let decoded = Envelope::decode(&bytes).unwrap();
            assert_eq!(peek_route(&bytes), Some(decoded.route));
            assert_eq!(peek_shard(&bytes), Some(decoded.route.shard()));
        }
    }

    #[test]
    fn peek_route_is_bounds_checked_on_short_buffers() {
        for len in 0..ROUTE_OFFSET.checked_add(4).unwrap() {
            let bytes = std::vec![0u8; len];
            assert_eq!(peek_route(&bytes), None);
            assert_eq!(peek_shard(&bytes), None);
        }
    }

    #[test]
    fn peek_route_ignores_bad_version_and_type() {
        let mut bytes =
            envelope(EnvelopeType::Media, RouteId::from_raw(0x2222_3333), 1, 1).encode();
        bytes[0] = 0xFF;
        bytes[1] = 0xFF;
        assert_eq!(peek_route(&bytes), Some(RouteId::from_raw(0x2222_3333)));
    }
}
