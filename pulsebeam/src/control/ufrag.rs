use crate::route::RouteId;
use str0m::IceCreds;

/// Wire layout — 10 bytes → 16 Crockford base32 chars (80 bits / 5 = 16, exact):
///
/// ```text
///  byte 0        byte 1       bytes 2-3     bytes 4-7      bytes 8-9
/// ┌────────────┬────────────┬────────────┬──────────────┬────────────┐
/// │ ver(4)     │ cluster_lo │  node_id   │  route (32)  │  epoch     │
/// │ clust_hi(4)│  (8 bits)  │ (16 bits)  │ shard|slot   │ (16 bits)  │
/// └────────────┴────────────┴────────────┴──────────────┴────────────┘
/// ```
///
/// Field ranges:
/// - **version**      4 bits  → 16 layout versions
/// - **cluster_id**  12 bits  → 4 095 clusters
/// - **node_id**     16 bits  → 65 535 nodes per cluster
/// - **route**        32 bits → the client's ICE association, `shard(12) | slot(20)`
/// - **epoch**        16 bits → the route's incarnation
///
/// The ufrag carries no identity at all — [`RouteId`] already encodes the
/// shard, so a separate `shard_id` field would be two sources of truth for
/// the same fact. `ParticipantId` doesn't appear here either: it is a
/// control-plane name that signaling uses to recognise the same person
/// across a reconnect, but the route and the participant's key are minted
/// together at connection setup and destroyed together at teardown, so
/// nothing on the data path needs to resolve one from the other. A
/// reconnect tears down the old connection and gets a new route for the new
/// ufrag; there is no stable identity here for it to survive under.
///
/// The ICE password is 15 random bytes → 24 Crockford chars (≥ RFC 8445 minimum of 22).
const VERSION: u8 = 0;
const RAW_LEN: usize = 10;
const PASS_RAW_LEN: usize = 15;
/// Exact length (in ASCII characters) of a v0 encoded ICE ufrag:
/// 10 bytes × 8 bits / 5 bits-per-Crockford-char = 16 chars, no padding.
pub const ENCODED_LEN: usize = 16;

/// Structured ICE ufrag that encodes all routing metadata needed to forward a
/// STUN binding request to the correct shard and route — without any
/// distributed lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IceUfrag {
    /// Which cluster this node belongs to.  0 for single-cluster deployments.
    pub cluster_id: u16,
    /// Which node within the cluster.  0 for single-node deployments.
    pub node_id: u16,
    /// The client's ICE association. Carries its own destination shard —
    /// see [`RouteId`] — so nothing else here needs to.
    pub route: RouteId,
    /// The route's incarnation, checked the same way every other arrival is.
    pub epoch: u16,
}

impl IceUfrag {
    /// Exact encoded length in ASCII characters (Crockford base32, no padding).
    pub const ENCODED_LEN: usize = ENCODED_LEN;

    pub fn new(cluster_id: u16, node_id: u16, route: RouteId, epoch: u16) -> Self {
        debug_assert!(
            cluster_id < 4096,
            "cluster_id must fit in 12 bits (max 4095)"
        );
        Self {
            cluster_id,
            node_id,
            route,
            epoch,
        }
    }

    /// Encode to a 16-character Crockford base32 string for use as an ICE ufrag.
    /// All output characters are in [A-Z0-9], valid `ice-char` per RFC 8445.
    pub fn encode(&self) -> String {
        let mut raw = [0u8; RAW_LEN];
        raw[0] = (VERSION << 4) | ((self.cluster_id >> 8) as u8 & 0x0f);
        raw[1] = (self.cluster_id & 0xff) as u8;
        raw[2..4].copy_from_slice(&self.node_id.to_be_bytes());
        raw[4..8].copy_from_slice(&self.route.get().to_be_bytes());
        raw[8..10].copy_from_slice(&self.epoch.to_be_bytes());
        base32::encode(base32::Alphabet::Crockford, &raw)
    }

    /// Decode from a Crockford base32 string.  Returns `None` if the string is
    /// the wrong length, malformed, or carries an unknown version number.
    pub fn decode(s: &str) -> Option<Self> {
        let raw = base32::decode(base32::Alphabet::Crockford, s)?;
        if raw.len() != RAW_LEN {
            return None;
        }
        let [c_hi, c_lo, n0, n1, r0, r1, r2, r3, e0, e1] = raw[..] else {
            return None;
        };
        if c_hi >> 4 != VERSION {
            return None;
        }
        let cluster_id = (u16::from(c_hi & 0x0f) << 8) | u16::from(c_lo);
        let node_id = u16::from_be_bytes([n0, n1]);
        let route = RouteId::from_raw(u32::from_be_bytes([r0, r1, r2, r3]));
        let epoch = u16::from_be_bytes([e0, e1]);
        Some(Self {
            cluster_id,
            node_id,
            route,
            epoch,
        })
    }

    /// Build complete `IceCreds` (encoded ufrag + random password) ready to
    /// pass to `rtc.direct_api().set_local_ice_credentials(...)`.
    pub fn into_ice_creds(self, rng: &mut impl pulsebeam_runtime::rand::RngCore) -> IceCreds {
        let mut pass_raw = [0u8; PASS_RAW_LEN];
        rng.fill_bytes(&mut pass_raw);
        let pass = base32::encode(base32::Alphabet::Crockford, &pass_raw);
        IceCreds {
            ufrag: self.encode(),
            pass,
        }
    }
}

#[cfg(test)]
mod tests {
    // Tests assert by panicking; the process ending is the mechanism.
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;
    use crate::id::ShardId;
    use pulsebeam_runtime::rand::os_rng;

    fn route(shard: usize, slot: u32) -> RouteId {
        RouteId::new(ShardId::new(shard), slot)
    }

    #[test]
    fn encode_length_is_16() {
        let u = IceUfrag::new(0, 0, route(0, 0), 0);
        assert_eq!(u.encode().len(), 16);
    }

    #[test]
    fn roundtrip() {
        let orig = IceUfrag::new(0xabc, 0x1234, route(7, 12345), 42);
        let decoded = IceUfrag::decode(&orig.encode()).unwrap();
        assert_eq!(decoded, orig);
    }

    #[test]
    fn decode_rejects_wrong_length() {
        assert!(IceUfrag::decode("TOOSHORT").is_none());
    }

    #[test]
    fn decode_rejects_unknown_version() {
        let u = IceUfrag::new(0, 0, route(0, 0), 0);
        let mut encoded = u.encode();
        // Flip the high nibble of the first char to make version != 0.
        // Crockford '1' encodes as 0x01, so replacing the first char with
        // a value whose high nibble is non-zero is simplest via raw bytes.
        let raw = base32::decode(base32::Alphabet::Crockford, &encoded).unwrap();
        let mut bad = raw;
        bad[0] = 0x10; // version = 1
        encoded = base32::encode(base32::Alphabet::Crockford, &bad);
        assert!(IceUfrag::decode(&encoded).is_none());
    }

    #[test]
    fn ice_creds_ufrag_and_pass_lengths() {
        let u = IceUfrag::new(1, 2, route(3, 9), 5);
        let creds = u.into_ice_creds(&mut os_rng());
        assert_eq!(creds.ufrag.len(), 16);
        assert_eq!(creds.pass.len(), 24); // 15 bytes → 24 Crockford chars
        assert!(creds.pass.len() >= 22); // RFC 8445 minimum
    }

    #[test]
    fn the_route_s_own_shard_survives_the_round_trip() {
        let u = IceUfrag::new(0, 0, route(9, 1), 0);
        let decoded = IceUfrag::decode(&u.encode()).unwrap();
        assert_eq!(decoded.route.shard(), ShardId::new(9));
    }
}
