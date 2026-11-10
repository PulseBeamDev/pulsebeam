//! RFC 9443 first-byte classification, not authentication or peer lookup.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PacketKind {
    Stun,
    Dtls,
    RtpRtcp,
    Quic,
    TurnChannel,
    Unknown,
}

/// TURN ChannelData takes precedence only for an explicitly known TURN source.
/// Different DTLS usages still require host-owned tuple/association context.
/// QUIC peers must keep the QUIC fixed bit set (disable bit greasing).
pub fn classify(packet: &[u8], known_turn_source: bool) -> PacketKind {
    match packet.first().copied() {
        Some(0..=3) => PacketKind::Stun,
        Some(20..=63) => PacketKind::Dtls,
        Some(64..=79) if known_turn_source => PacketKind::TurnChannel,
        Some(64..=127 | 192..=255) => PacketKind::Quic,
        Some(128..=191) => PacketKind::RtpRtcp,
        _ => PacketKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixed_bit_classification_requires_explicit_turn_source_context() {
        assert_eq!(classify(&[], false), PacketKind::Unknown);
        for first in 0u8..=u8::MAX {
            let ordinary = classify(&[first], false);
            let turn = classify(&[first], true);
            if (64..=79).contains(&first) {
                assert_eq!(ordinary, PacketKind::Quic);
                assert_eq!(turn, PacketKind::TurnChannel);
            } else {
                assert_eq!(ordinary, turn);
            }
            if (20..=63).contains(&first) {
                assert_eq!(ordinary, PacketKind::Dtls);
            }
            if (128..=191).contains(&first) {
                assert_eq!(ordinary, PacketKind::RtpRtcp);
            }
            if first >= 192 {
                assert_eq!(ordinary, PacketKind::Quic);
            }
        }
    }
}
