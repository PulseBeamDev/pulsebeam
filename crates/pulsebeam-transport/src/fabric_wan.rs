//! PRODUCTION WIRE CONTRACT — EDIT WITH EXTREME CARE. Changes can break deployed clients, mixed-version nodes, and production traffic. Require compatibility review and wire-vector tests.
//!
//! Fabric WAN SRTP capability, [RFC #275 §9.1][rfc]. Six octets: scheme
//! `03 01`, operation (1 offer, 2 accept), revision 0, and DTLS-SRTP
//! profile `00 08` in network byte order. The literal test vectors below
//! pin the compatibility boundary with the historical DTLS carrier.
//!
//! The association owner must authenticate the control payload and verify the
//! actual negotiated exporter profile, role and exchange state. Parsing these
//! bytes alone does not enable media. Never use this codec to sniff a carrier.
//!
//! [rfc]: https://github.com/PulseBeamDev/pulsebeam/issues/275

const OFFER: [u8; 6] = [3, 1, 1, 0, 0, 8];
const ACCEPT: [u8; 6] = [3, 1, 2, 0, 0, 8];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Capability {
    Offer,
    Accept,
}

impl Capability {
    pub const fn encode(self) -> [u8; 6] {
        match self {
            Self::Offer => OFFER,
            Self::Accept => ACCEPT,
        }
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let image: [u8; 6] = bytes.try_into().ok()?;
        match image {
            OFFER => Some(Self::Offer),
            ACCEPT => Some(Self::Accept),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Capability;

    #[test]
    fn capability_has_exact_offer_and_accept_images() {
        for (message, expected) in [
            (Capability::Offer, [0x03, 0x01, 0x01, 0x00, 0x00, 0x08]),
            (Capability::Accept, [0x03, 0x01, 0x02, 0x00, 0x00, 0x08]),
        ] {
            assert_eq!(message.encode(), expected);
            assert_eq!(Capability::decode(&expected), Some(message));
            #[cfg(feature = "datagram")]
            assert_eq!(
                crate::wire::decode_for(&expected, true).err(),
                Some(crate::wire::InvalidFrame)
            );
        }
    }

    #[test]
    fn capability_rejects_incompatible_scheme_revision_profile_and_length() {
        let invalid: &[&[u8]] = &[
            &[2, 1, 1, 4, 128],
            &[2, 1, 2, 4, 128],
            &[3, 2, 1, 0, 0, 8],
            &[3, 1, 0, 0, 0, 8],
            &[3, 1, 1, 1, 0, 8],
            &[3, 1, 2, 0, 0, 7],
            &[3, 1, 1, 0, 0],
            &[3, 1, 1, 0, 0, 8, 0],
        ];
        for image in invalid {
            assert_eq!(Capability::decode(image), None);
        }
    }
}
