//! Bounded packet classifiers. These are the exact functions the Aya eBPF
//! program calls to decide which shard socket owns a packet — everything
//! here must stay verifier-friendly: bounded traversal, bounds-checked
//! reads, no allocation.

use crate::envelope::{Envelope, EnvelopeError};
use crate::ufrag::{self, UfragDecodeError};
use crate::{stun, TransportHandle};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    NotStun,
    MalformedStun,
    NoUsername,
    BadUfragLen,
    BadUfragEncoding,
    BadVersion,
    WrongCluster,
    WrongNode,
    MalformedEnvelope,
    UnknownEnvelopeType,
    InvalidShard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientVerdict {
    Drop(DropReason),
    /// First STUN of a connection: steer by the ufrag's transport route.
    Bootstrap {
        handle: TransportHandle,
        cluster_id: u16,
        node_id: u16,
    },
    /// Not STUN — an established flow; steering comes from flow affinity.
    Established,
}

/// Bounded STUN classifier. `payload` is the UDP payload.
pub fn classify_client(payload: &[u8]) -> ClientVerdict {
    if !stun::is_stun(payload) {
        return ClientVerdict::Established;
    }
    let Some(username) = stun::server_ufrag(payload) else {
        return ClientVerdict::Drop(DropReason::NoUsername);
    };
    match decode_ufrag_token(username) {
        Ok(u) => ClientVerdict::Bootstrap {
            handle: TransportHandle {
                route: u.transport,
                epoch: u.epoch,
            },
            cluster_id: u.cluster_id,
            node_id: u.node_id,
        },
        Err(reason) => ClientVerdict::Drop(reason),
    }
}

/// Same as [`classify_client`], but also checks cluster/node and that the
/// resolved shard is within `shard_count`.
pub fn classify_client_for_node(
    payload: &[u8],
    cluster_id: u16,
    node_id: u16,
    shard_count: u16,
) -> ClientVerdict {
    match classify_client(payload) {
        ClientVerdict::Bootstrap {
            handle,
            cluster_id: got_cluster,
            node_id: got_node,
        } => {
            if got_cluster != cluster_id {
                return ClientVerdict::Drop(DropReason::WrongCluster);
            }
            if got_node != node_id {
                return ClientVerdict::Drop(DropReason::WrongNode);
            }
            if handle.route.shard() >= shard_count {
                return ClientVerdict::Drop(DropReason::InvalidShard);
            }
            ClientVerdict::Bootstrap {
                handle,
                cluster_id: got_cluster,
                node_id: got_node,
            }
        }
        other => other,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeVerdict {
    Drop(DropReason),
    Steer { shard: u16 },
}

/// Reads the fixed-offset Envelope route and resolves the destination shard.
pub fn classify_node(payload: &[u8], shard_count: u16) -> NodeVerdict {
    match Envelope::decode(payload) {
        Ok(env) => {
            let shard = env.route.shard();
            if shard >= shard_count {
                NodeVerdict::Drop(DropReason::InvalidShard)
            } else {
                NodeVerdict::Steer { shard }
            }
        }
        Err(EnvelopeError::Truncated { .. }) => NodeVerdict::Drop(DropReason::MalformedEnvelope),
        Err(EnvelopeError::UnsupportedVersion { .. }) => NodeVerdict::Drop(DropReason::BadVersion),
        Err(EnvelopeError::UnknownType { .. }) => {
            NodeVerdict::Drop(DropReason::UnknownEnvelopeType)
        }
    }
}

fn decode_ufrag_token(token: &[u8]) -> Result<ufrag::IceUfrag, DropReason> {
    if token.len() != ufrag::ENCODED_LEN {
        return Err(DropReason::BadUfragLen);
    }
    ufrag::decode_ascii_detailed(token).map_err(|e| match e {
        UfragDecodeError::BadEncoding => DropReason::BadUfragEncoding,
        UfragDecodeError::BadVersion => DropReason::BadVersion,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::EnvelopeType;
    use crate::ufrag::IceUfrag;
    use crate::{RouteId, TransportRoute};
    use std::vec::Vec;

    const BINDING_REQUEST: u16 = 0x0001;
    const MAGIC_COOKIE_BYTES: [u8; 4] = crate::stun::MAGIC_COOKIE.to_be_bytes();
    const USERNAME_ATTRIBUTE_TYPE: u16 = 0x0006;
    const MIN_STUN_HEADER_SIZE: usize = 20;

    fn build_stun_with_username(value: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
        buf.extend_from_slice(&[0u8; 2]);
        buf.extend_from_slice(&MAGIC_COOKIE_BYTES);
        buf.extend_from_slice(&[0u8; 12]);
        assert_eq!(buf.len(), MIN_STUN_HEADER_SIZE);

        let padded_len = (value.len() + 3) & !3;
        let padding = padded_len - value.len();
        buf.extend_from_slice(&USERNAME_ATTRIBUTE_TYPE.to_be_bytes());
        buf.extend_from_slice(&u16::try_from(value.len()).unwrap().to_be_bytes());
        buf.extend_from_slice(value);
        buf.extend_from_slice(&std::vec![0u8; padding]);

        let total_attr_len = u16::try_from(4 + padded_len).unwrap();
        buf[2..4].copy_from_slice(&total_attr_len.to_be_bytes());
        buf
    }

    fn ufrag_username(u: &IceUfrag) -> Vec<u8> {
        let mut v = Vec::from(u.encode_ascii());
        v.push(b':');
        v.extend_from_slice(b"client");
        v
    }

    #[test]
    fn non_stun_is_established() {
        let payload = std::vec![0xAAu8; 64];
        assert_eq!(classify_client(&payload), ClientVerdict::Established);
    }

    #[test]
    fn stun_without_username_is_dropped() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
        buf.extend_from_slice(&0u16.to_be_bytes());
        buf.extend_from_slice(&MAGIC_COOKIE_BYTES);
        buf.extend_from_slice(&[0u8; 12]);
        assert_eq!(
            classify_client(&buf),
            ClientVerdict::Drop(DropReason::NoUsername)
        );
    }

    #[test]
    fn valid_bootstrap_ufrag_resolves_transport_handle() {
        let u = IceUfrag::new(3, 5, TransportRoute::new(2, 100), 7);
        let msg = build_stun_with_username(&ufrag_username(&u));
        assert_eq!(
            classify_client(&msg),
            ClientVerdict::Bootstrap {
                handle: TransportHandle {
                    route: u.transport,
                    epoch: u.epoch,
                },
                cluster_id: u.cluster_id,
                node_id: u.node_id,
            }
        );
    }

    #[test]
    fn wrong_length_ufrag_token_is_bad_ufrag_len() {
        let msg = build_stun_with_username(b"short:client");
        assert_eq!(
            classify_client(&msg),
            ClientVerdict::Drop(DropReason::BadUfragLen)
        );
    }

    #[test]
    fn invalid_crockford_chars_are_bad_ufrag_encoding() {
        let mut token = std::vec![b'*'; ufrag::ENCODED_LEN];
        token.push(b':');
        token.extend_from_slice(b"client");
        let msg = build_stun_with_username(&token);
        assert_eq!(
            classify_client(&msg),
            ClientVerdict::Drop(DropReason::BadUfragEncoding)
        );
    }

    #[test]
    fn wrong_version_ufrag_is_bad_version() {
        let u = IceUfrag::new(0, 0, TransportRoute::new(0, 0), 0);
        let mut raw = u.encode_raw();
        raw[0] = 0x10;
        let ascii = crate::ufrag::encode_ascii_raw_bytes(&raw);
        let mut token = Vec::from(ascii);
        token.push(b':');
        token.extend_from_slice(b"client");
        let msg = build_stun_with_username(&token);
        assert_eq!(
            classify_client(&msg),
            ClientVerdict::Drop(DropReason::BadVersion)
        );
    }

    #[test]
    fn classify_for_node_checks_cluster_node_and_shard_bound() {
        let u = IceUfrag::new(3, 5, TransportRoute::new(2, 100), 7);
        let msg = build_stun_with_username(&ufrag_username(&u));

        assert_eq!(
            classify_client_for_node(&msg, 3, 5, 10),
            ClientVerdict::Bootstrap {
                handle: TransportHandle {
                    route: u.transport,
                    epoch: u.epoch,
                },
                cluster_id: 3,
                node_id: 5,
            }
        );
        assert_eq!(
            classify_client_for_node(&msg, 99, 5, 10),
            ClientVerdict::Drop(DropReason::WrongCluster)
        );
        assert_eq!(
            classify_client_for_node(&msg, 3, 99, 10),
            ClientVerdict::Drop(DropReason::WrongNode)
        );
        assert_eq!(
            classify_client_for_node(&msg, 3, 5, 2),
            ClientVerdict::Drop(DropReason::InvalidShard)
        );
    }

    #[test]
    fn classify_node_steers_by_shard() {
        let env = Envelope {
            ty: EnvelopeType::Media,
            epoch: 1,
            route: RouteId::new(4, 10),
            extension: 0,
        };
        let bytes = env.encode();
        assert_eq!(classify_node(&bytes, 8), NodeVerdict::Steer { shard: 4 });
        assert_eq!(
            classify_node(&bytes, 4),
            NodeVerdict::Drop(DropReason::InvalidShard)
        );
    }

    #[test]
    fn classify_node_reports_malformed_and_unknown_type() {
        assert_eq!(
            classify_node(&[0u8; 4], 8),
            NodeVerdict::Drop(DropReason::MalformedEnvelope)
        );

        let env = Envelope {
            ty: EnvelopeType::Media,
            epoch: 1,
            route: RouteId::new(0, 0),
            extension: 0,
        };
        let mut bytes = env.encode();
        bytes[1] = 250;
        assert_eq!(
            classify_node(&bytes, 8),
            NodeVerdict::Drop(DropReason::UnknownEnvelopeType)
        );

        bytes = env.encode();
        bytes[0] = 250;
        assert_eq!(
            classify_node(&bytes, 8),
            NodeVerdict::Drop(DropReason::BadVersion)
        );
    }
}
