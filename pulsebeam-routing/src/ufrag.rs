//! The ICE ufrag wire format: 10 raw bytes, Crockford base32-encoded to a
//! fixed 16 ASCII characters.
//!
//! ```text
//! ver(4) | cluster_id(12) | node_id(16) | transport_route(32) | epoch(16)
//! ```
//!
//! Mirrors `pulsebeam/src/control/ufrag.rs`'s byte layout exactly. The
//! Crockford base32 codec here is a fixed-size, allocation-free
//! reimplementation of the `base32` crate's `Alphabet::Crockford` — see
//! `differential` tests for the equivalence proof.

use crate::TransportRoute;

pub const VERSION: u8 = 0;
pub const RAW_LEN: usize = 10;
pub const ENCODED_LEN: usize = 16;

const CROCKFORD_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IceUfrag {
    pub cluster_id: u16,
    pub node_id: u16,
    pub transport: TransportRoute,
    pub epoch: u16,
}

pub(crate) enum UfragDecodeError {
    BadEncoding,
    BadVersion,
}

impl IceUfrag {
    pub const fn new(cluster_id: u16, node_id: u16, transport: TransportRoute, epoch: u16) -> Self {
        debug_assert!(
            cluster_id <= 0x0FFF,
            "cluster_id must fit in 12 bits (max 4095)"
        );
        Self {
            cluster_id,
            node_id,
            transport,
            epoch,
        }
    }

    pub fn encode_raw(&self) -> [u8; RAW_LEN] {
        let cluster_bytes = self.cluster_id.to_be_bytes();
        let node_bytes = self.node_id.to_be_bytes();
        let route_bytes = self.transport.get().to_be_bytes();
        let epoch_bytes = self.epoch.to_be_bytes();
        [
            (VERSION << 4) | (cluster_bytes[0] & 0x0F),
            cluster_bytes[1],
            node_bytes[0],
            node_bytes[1],
            route_bytes[0],
            route_bytes[1],
            route_bytes[2],
            route_bytes[3],
            epoch_bytes[0],
            epoch_bytes[1],
        ]
    }

    pub fn encode_ascii(&self) -> [u8; ENCODED_LEN] {
        encode_ascii_raw_bytes(&self.encode_raw())
    }

    pub fn decode_raw(raw: &[u8]) -> Option<Self> {
        let arr: [u8; RAW_LEN] = raw.try_into().ok()?;
        decode_raw_detailed(&arr).ok()
    }

    pub fn decode_ascii(s: &[u8]) -> Option<Self> {
        decode_ascii_detailed(s).ok()
    }
}

pub(crate) fn decode_ascii_detailed(s: &[u8]) -> Result<IceUfrag, UfragDecodeError> {
    let arr: [u8; ENCODED_LEN] = s.try_into().map_err(|_| UfragDecodeError::BadEncoding)?;
    let [c0, c1, c2, c3, c4, c5, c6, c7, c8, c9, c10, c11, c12, c13, c14, c15] = arr;
    let v0 = crockford_value(c0).ok_or(UfragDecodeError::BadEncoding)?;
    let v1 = crockford_value(c1).ok_or(UfragDecodeError::BadEncoding)?;
    let v2 = crockford_value(c2).ok_or(UfragDecodeError::BadEncoding)?;
    let v3 = crockford_value(c3).ok_or(UfragDecodeError::BadEncoding)?;
    let v4 = crockford_value(c4).ok_or(UfragDecodeError::BadEncoding)?;
    let v5 = crockford_value(c5).ok_or(UfragDecodeError::BadEncoding)?;
    let v6 = crockford_value(c6).ok_or(UfragDecodeError::BadEncoding)?;
    let v7 = crockford_value(c7).ok_or(UfragDecodeError::BadEncoding)?;
    let v8 = crockford_value(c8).ok_or(UfragDecodeError::BadEncoding)?;
    let v9 = crockford_value(c9).ok_or(UfragDecodeError::BadEncoding)?;
    let v10 = crockford_value(c10).ok_or(UfragDecodeError::BadEncoding)?;
    let v11 = crockford_value(c11).ok_or(UfragDecodeError::BadEncoding)?;
    let v12 = crockford_value(c12).ok_or(UfragDecodeError::BadEncoding)?;
    let v13 = crockford_value(c13).ok_or(UfragDecodeError::BadEncoding)?;
    let v14 = crockford_value(c14).ok_or(UfragDecodeError::BadEncoding)?;
    let v15 = crockford_value(c15).ok_or(UfragDecodeError::BadEncoding)?;

    let [b0, b1, b2, b3, b4] = decode_chunk([v0, v1, v2, v3, v4, v5, v6, v7]);
    let [b5, b6, b7, b8, b9] = decode_chunk([v8, v9, v10, v11, v12, v13, v14, v15]);
    decode_raw_detailed(&[b0, b1, b2, b3, b4, b5, b6, b7, b8, b9])
}

pub(crate) fn encode_ascii_raw_bytes(raw: &[u8; RAW_LEN]) -> [u8; ENCODED_LEN] {
    let [b0, b1, b2, b3, b4, b5, b6, b7, b8, b9] = *raw;
    let [a0, a1, a2, a3, a4, a5, a6, a7] = encode_chunk(b0, b1, b2, b3, b4);
    let [a8, a9, a10, a11, a12, a13, a14, a15] = encode_chunk(b5, b6, b7, b8, b9);
    [
        a0, a1, a2, a3, a4, a5, a6, a7, a8, a9, a10, a11, a12, a13, a14, a15,
    ]
}

fn decode_raw_detailed(raw: &[u8; RAW_LEN]) -> Result<IceUfrag, UfragDecodeError> {
    let [c_hi, c_lo, n0, n1, r0, r1, r2, r3, e0, e1] = *raw;
    if c_hi >> 4 != VERSION {
        return Err(UfragDecodeError::BadVersion);
    }
    let cluster_id = (u16::from(c_hi & 0x0F) << 8) | u16::from(c_lo);
    let node_id = u16::from_be_bytes([n0, n1]);
    let transport = TransportRoute::from_raw(u32::from_be_bytes([r0, r1, r2, r3]));
    let epoch = u16::from_be_bytes([e0, e1]);
    Ok(IceUfrag {
        cluster_id,
        node_id,
        transport,
        epoch,
    })
}

#[allow(
    clippy::indexing_slicing,
    reason = "value is masked to 5 bits (0..=31); CROCKFORD_ALPHABET has exactly 32 entries"
)]
const fn crockford_char(value: u8) -> u8 {
    CROCKFORD_ALPHABET[(value & 0x1F) as usize]
}

const fn crockford_value(c: u8) -> Option<u8> {
    match c {
        b'0' | b'O' | b'o' => Some(0),
        b'1' | b'I' | b'i' | b'L' | b'l' => Some(1),
        b'2' => Some(2),
        b'3' => Some(3),
        b'4' => Some(4),
        b'5' => Some(5),
        b'6' => Some(6),
        b'7' => Some(7),
        b'8' => Some(8),
        b'9' => Some(9),
        b'A' | b'a' => Some(10),
        b'B' | b'b' => Some(11),
        b'C' | b'c' => Some(12),
        b'D' | b'd' => Some(13),
        b'E' | b'e' => Some(14),
        b'F' | b'f' => Some(15),
        b'G' | b'g' => Some(16),
        b'H' | b'h' => Some(17),
        b'J' | b'j' => Some(18),
        b'K' | b'k' => Some(19),
        b'M' | b'm' => Some(20),
        b'N' | b'n' => Some(21),
        b'P' | b'p' => Some(22),
        b'Q' | b'q' => Some(23),
        b'R' | b'r' => Some(24),
        b'S' | b's' => Some(25),
        b'T' | b't' => Some(26),
        b'V' | b'v' => Some(27),
        b'W' | b'w' => Some(28),
        b'X' | b'x' => Some(29),
        b'Y' | b'y' => Some(30),
        b'Z' | b'z' => Some(31),
        _ => None,
    }
}

fn encode_chunk(b0: u8, b1: u8, b2: u8, b3: u8, b4: u8) -> [u8; 8] {
    [
        crockford_char(b0 >> 3),
        crockford_char(((b0 & 0x07) << 2) | (b1 >> 6)),
        crockford_char((b1 >> 1) & 0x1F),
        crockford_char(((b1 & 0x01) << 4) | (b2 >> 4)),
        crockford_char(((b2 & 0x0F) << 1) | (b3 >> 7)),
        crockford_char((b3 >> 2) & 0x1F),
        crockford_char(((b3 & 0x03) << 3) | (b4 >> 5)),
        crockford_char(b4 & 0x1F),
    ]
}

fn decode_chunk(v: [u8; 8]) -> [u8; 5] {
    let [v0, v1, v2, v3, v4, v5, v6, v7] = v;
    [
        (v0 << 3) | (v1 >> 2),
        (v1 << 6) | (v2 << 1) | (v3 >> 4),
        (v3 << 4) | (v4 >> 1),
        (v4 << 7) | (v5 << 2) | (v6 >> 3),
        (v6 << 5) | v7,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::String;
    use std::vec::Vec;

    fn transport(shard: u16, slot: u32) -> TransportRoute {
        TransportRoute::new(shard, slot)
    }

    fn ascii_to_string(bytes: [u8; ENCODED_LEN]) -> String {
        String::from_utf8(Vec::from(bytes)).unwrap()
    }

    #[test]
    fn encode_ascii_is_16_chars() {
        let u = IceUfrag::new(0, 0, transport(0, 0), 0);
        assert_eq!(u.encode_ascii().len(), ENCODED_LEN);
    }

    #[test]
    fn round_trips_min_and_max_values() {
        for u in [
            IceUfrag::new(0, 0, transport(0, 0), 0),
            IceUfrag::new(
                0x0FFF,
                u16::MAX,
                TransportRoute::new(
                    u16::try_from(TransportRoute::MAX_SHARD).unwrap(),
                    TransportRoute::MAX_SLOT,
                ),
                u16::MAX,
            ),
            IceUfrag::new(0xabc, 0x1234, transport(7, 12345), 42),
        ] {
            let ascii = u.encode_ascii();
            assert_eq!(IceUfrag::decode_ascii(&ascii), Some(u));

            let raw = u.encode_raw();
            assert_eq!(IceUfrag::decode_raw(&raw), Some(u));
        }
    }

    #[test]
    fn decode_ascii_rejects_wrong_length() {
        assert!(IceUfrag::decode_ascii(b"TOOSHORT").is_none());
        assert!(IceUfrag::decode_ascii(b"WAYTOOLONGTOBEAVALIDUFRAGSTRING").is_none());
        assert!(IceUfrag::decode_ascii(b"").is_none());
    }

    #[test]
    fn decode_raw_rejects_wrong_length() {
        assert!(IceUfrag::decode_raw(&[0u8; RAW_LEN - 1]).is_none());
        assert!(IceUfrag::decode_raw(&[0u8; RAW_LEN + 1]).is_none());
    }

    #[test]
    fn decode_ascii_rejects_invalid_chars() {
        let u = IceUfrag::new(0, 0, transport(0, 0), 0);
        let mut ascii = u.encode_ascii();
        ascii[0] = b'U'; // 'U' is deliberately excluded from Crockford
        assert!(IceUfrag::decode_ascii(&ascii).is_none());
        ascii[0] = b'*';
        assert!(IceUfrag::decode_ascii(&ascii).is_none());
    }

    #[test]
    fn decode_rejects_unknown_version() {
        let u = IceUfrag::new(0, 0, transport(0, 0), 0);
        let mut raw = u.encode_raw();
        raw[0] = 0x10; // version = 1
        assert!(IceUfrag::decode_raw(&raw).is_none());

        let ascii = encode_ascii_raw_bytes(&raw);
        assert!(IceUfrag::decode_ascii(&ascii).is_none());
    }

    #[test]
    fn differential_matches_base32_crate_for_many_inputs() {
        let mut state: u64 = 0x243F_6A88_85A3_08D3;
        for _ in 0..2000 {
            let mut raw = [0u8; RAW_LEN];
            for byte in &mut raw {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                *byte = (state >> 33) as u8;
            }
            raw[0] &= 0x0F; // keep version nibble at 0 so decode also round-trips

            let ours = encode_ascii_raw_bytes(&raw);
            let theirs = base32::encode(base32::Alphabet::Crockford, &raw);
            assert_eq!(ascii_to_string(ours), theirs);

            let decoded_theirs = base32::decode(base32::Alphabet::Crockford, &theirs).unwrap();
            assert_eq!(decoded_theirs.as_slice(), &raw);
        }
    }

    #[test]
    fn ufrag_symmetry_survives_full_round_trip_through_the_ascii_wire() {
        let u = IceUfrag::new(0x0AB, 0x00CD, transport(9, 1), 3);
        let ascii = u.encode_ascii();
        let s = ascii_to_string(ascii);
        let decoded = IceUfrag::decode_ascii(s.as_bytes()).unwrap();
        assert_eq!(decoded, u);
        assert_eq!(decoded.transport.shard(), 9);
    }
}
