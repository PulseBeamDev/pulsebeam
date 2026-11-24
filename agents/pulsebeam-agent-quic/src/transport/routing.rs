//! Stateless shard hints in native QUIC destination connection IDs.

use std::time::Duration;

use noq_proto::{ConnectionId, ConnectionIdGenerator, InvalidCid, RandomConnectionIdGenerator};

use super::{MAX_V1_CID_BYTES, UDP_PAYLOAD_BYTES};

pub use pulsebeam_proto::NATIVE_QUIC_INITIAL_CID_BYTES as ROUTED_CID_BYTES;
const _: () = assert!(ROUTED_CID_BYTES <= MAX_V1_CID_BYTES);
const ROUTED_LENGTH_BYTE: u8 = 20;
const _: () = assert!(ROUTED_CID_BYTES == ROUTED_LENGTH_BYTE as usize);

/// Mint an Initial destination CID for a placement or an established CID
/// for the shard's Noq endpoint. The shard prefix is never an admission proof.
pub fn mint(shard: u16) -> [u8; ROUTED_CID_BYTES] {
    let random = RandomConnectionIdGenerator::new(ROUTED_CID_BYTES).generate_cid();
    let mut bytes = [0; ROUTED_CID_BYTES];
    bytes.copy_from_slice(&random);
    let [high, low, ..] = &mut bytes;
    [*high, *low] = shard.to_be_bytes();
    bytes
}

/// Install the exact destination CID returned by the HTTPS placement.
#[allow(
    clippy::disallowed_types,
    reason = "Noq requires a shared immutable Initial CID provider"
)]
pub fn set_initial_destination(config: &mut noq_proto::ClientConfig, cid: [u8; ROUTED_CID_BYTES]) {
    let cid = ConnectionId::new(&cid);
    config.initial_dst_cid_provider(std::sync::Arc::new(move || cid));
}

/// Read only enough of a native QUIC v1 header to select a shard. Noq still
/// validates the packet and the application still redeems its placement.
pub fn destination(payload: &[u8], shard_count: usize) -> Option<u16> {
    if payload.len() > usize::from(UDP_PAYLOAD_BYTES) {
        return None;
    }
    let cid = match payload {
        [0x40..=0x7f, body @ ..] => {
            let (cid, protected) = body.split_at_checked(ROUTED_CID_BYTES)?;
            // Noq requires a four-byte packet-number sample offset plus the AEAD sample.
            if protected.len() < 20 {
                return None;
            }
            cid
        }
        [
            first @ 0xc0..=0xff,
            0,
            0,
            0,
            1,
            ROUTED_LENGTH_BYTE,
            body @ ..,
        ] => {
            let (cid, rest) = body.split_at_checked(ROUTED_CID_BYTES)?;
            let (&source_len, rest) = rest.split_first()?;
            let source_len = usize::from(source_len);
            let packet_type = first & 0x30;
            if source_len > MAX_V1_CID_BYTES || rest.len() < source_len.saturating_add(21) {
                return None;
            }
            // The server only receives Initial and Handshake on this profile.
            if !matches!(packet_type, 0 | 0x20)
                || packet_type == 0 && payload.len() < usize::from(UDP_PAYLOAD_BYTES)
            {
                return None;
            }
            cid
        }
        _ => return None,
    };
    let &[high, low] = cid.first_chunk::<2>()?;
    let shard = u16::from_be_bytes([high, low]);
    (usize::from(shard) < shard_count).then_some(shard)
}

pub(super) struct Generator(pub(super) u16);

impl ConnectionIdGenerator for Generator {
    fn generate_cid(&mut self) -> ConnectionId {
        ConnectionId::new(&mint(self.0))
    }

    fn validate(&self, cid: ConnectionId) -> Result<(), InvalidCid> {
        if cid.len() != ROUTED_CID_BYTES || cid.first_chunk::<2>() != Some(&self.0.to_be_bytes()) {
            return Err(InvalidCid);
        }
        Ok(())
    }

    fn cid_len(&self) -> usize {
        ROUTED_CID_BYTES
    }

    fn cid_lifetime(&self) -> Option<Duration> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_and_rotated_cids_route_without_address_or_association_state() {
        let shard = 257;
        let placed = mint(shard);
        let mut initial = vec![0xc0, 0, 0, 0, 1, ROUTED_LENGTH_BYTE];
        initial.extend(placed);
        initial.extend([0; 20]);
        initial.resize(usize::from(UDP_PAYLOAD_BYTES), 0);
        assert_eq!(destination(&initial, 258), Some(shard));
        assert_eq!(destination(&initial, 257), None);
        let mut truncated = initial.clone();
        truncated.pop();
        assert_eq!(destination(&truncated, 258), None);
        for index in 0..6 {
            let mut malformed = initial.clone();
            *malformed.get_mut(index).unwrap() = 9;
            assert_eq!(destination(&malformed, 258), None);
        }

        let mut generator = Generator(shard);
        let first = generator.generate_cid();
        let rotated = generator.generate_cid();
        for cid in [first, rotated] {
            assert!(generator.validate(cid).is_ok());
            assert!(Generator(shard.saturating_sub(1)).validate(cid).is_err());
            let mut short = vec![0x40];
            short.extend_from_slice(&cid);
            assert_eq!(destination(&short, 258), None);
            short.extend([0; 20]);
            assert_eq!(destination(&short, 258), Some(shard));
            assert_eq!(destination(&short, 257), None);
        }
        assert_ne!(first, rotated);
    }
}
