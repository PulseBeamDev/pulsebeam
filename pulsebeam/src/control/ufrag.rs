use crate::entity::ParticipantId;
use str0m::IceCreds;

/// Wire layout — 25 bytes → 40 Crockford base32 chars (200 bits / 5 = 40, exact):
///
/// ```text
///  byte 0        byte 1       bytes 2-3     byte 4      bytes 5-8     bytes 9-24
/// ┌────────────┬────────────┬────────────┬────────────┬────────────┬──────────────┐
/// │ ver(4)     │ cluster_lo │  node_id   │  shard_id  │  reserved  │ participant  │
/// │ clust_hi(4)│  (8 bits)  │ (16 bits)  │  (8 bits)  │  (32 bits) │  id (128 b) │
/// └────────────┴────────────┴────────────┴────────────┴────────────┴──────────────┘
/// ```
///
/// Field ranges:
/// - **version**      4 bits  → 16 layout versions
/// - **cluster_id**  12 bits  → 4 095 clusters
/// - **node_id**     16 bits  → 65 535 nodes per cluster
/// - **shard_id**     8 bits  → 255 shards per node (one per CPU core)
/// - **reserved**    32 bits  → must be zero; available for `connection_seq` etc.
/// - **participant_id** 128 bits → stable UUID that survives ICE restarts
///
/// The ICE password is 15 random bytes → 24 Crockford chars (≥ RFC 8445 minimum of 22).
const VERSION: u8 = 0;
const RAW_LEN: usize = 25;
const PASS_RAW_LEN: usize = 15;

/// Structured ICE ufrag that encodes all routing metadata needed to forward a
/// STUN binding request to the correct shard — and in future, the correct
/// node — without any distributed lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IceUfrag {
    /// Which cluster this node belongs to.  0 for single-cluster deployments.
    pub cluster_id: u16,
    /// Which node within the cluster.  0 for single-node deployments.
    pub node_id: u16,
    /// Which shard (thread-per-core worker) on this node owns the participant.
    pub shard_id: u8,
    /// Stable participant identity that survives reconnects / ICE restarts.
    pub participant_id: ParticipantId,
}

impl IceUfrag {
    pub fn new(
        cluster_id: u16,
        node_id: u16,
        shard_id: u8,
        participant_id: ParticipantId,
    ) -> Self {
        debug_assert!(cluster_id < 4096, "cluster_id must fit in 12 bits (max 4095)");
        Self {
            cluster_id,
            node_id,
            shard_id,
            participant_id,
        }
    }

    /// Encode to a 40-character Crockford base32 string for use as an ICE ufrag.
    /// All output characters are in [A-Z0-9], valid `ice-char` per RFC 8445.
    pub fn encode(&self) -> String {
        let mut raw = [0u8; RAW_LEN];
        raw[0] = (VERSION << 4) | ((self.cluster_id >> 8) as u8 & 0x0f);
        raw[1] = (self.cluster_id & 0xff) as u8;
        raw[2..4].copy_from_slice(&self.node_id.to_be_bytes());
        raw[4] = self.shard_id;
        // bytes 5-8: reserved, already zero
        raw[9..25].copy_from_slice(self.participant_id.as_bytes());
        base32::encode(base32::Alphabet::Crockford, &raw)
    }

    /// Decode from a Crockford base32 string.  Returns `None` if the string is
    /// the wrong length, malformed, or carries an unknown version number.
    pub fn decode(s: &str) -> Option<Self> {
        let raw = base32::decode(base32::Alphabet::Crockford, s)?;
        if raw.len() != RAW_LEN {
            return None;
        }
        let version = raw[0] >> 4;
        if version != VERSION {
            return None;
        }
        let cluster_id = (((raw[0] & 0x0f) as u16) << 8) | (raw[1] as u16);
        let node_id = u16::from_be_bytes([raw[2], raw[3]]);
        let shard_id = raw[4];
        let participant_id =
            ParticipantId::from_bytes(raw[9..25].try_into().ok()?);
        Some(Self {
            cluster_id,
            node_id,
            shard_id,
            participant_id,
        })
    }

    /// Build complete `IceCreds` (encoded ufrag + random password) ready to
    /// pass to `rtc.direct_api().set_local_ice_credentials(...)`.
    pub fn into_ice_creds(
        self,
        rng: &mut impl pulsebeam_runtime::rand::RngCore,
    ) -> IceCreds {
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
    use super::*;
    use pulsebeam_runtime::rand::os_rng;

    fn dummy_participant() -> ParticipantId {
        ParticipantId::from_bytes([
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
        ])
    }

    #[test]
    fn encode_length_is_40() {
        let u = IceUfrag::new(0, 0, 0, dummy_participant());
        assert_eq!(u.encode().len(), 40);
    }

    #[test]
    fn roundtrip() {
        let orig = IceUfrag::new(0xabc, 0x1234, 7, dummy_participant());
        let decoded = IceUfrag::decode(&orig.encode()).unwrap();
        assert_eq!(decoded, orig);
    }

    #[test]
    fn decode_rejects_wrong_length() {
        assert!(IceUfrag::decode("TOOSHORT").is_none());
    }

    #[test]
    fn decode_rejects_unknown_version() {
        let u = IceUfrag::new(0, 0, 0, dummy_participant());
        let mut encoded = u.encode();
        // Flip the high nibble of the first char to make version != 0.
        // Crockford '1' encodes as 0x01, so replacing the first char with
        // a value whose high nibble is non-zero is simplest via raw bytes.
        let raw = base32::decode(base32::Alphabet::Crockford, &encoded).unwrap();
        let mut bad = raw.clone();
        bad[0] = 0x10; // version = 1
        encoded = base32::encode(base32::Alphabet::Crockford, &bad);
        assert!(IceUfrag::decode(&encoded).is_none());
    }

    #[test]
    fn ice_creds_ufrag_and_pass_lengths() {
        let u = IceUfrag::new(1, 2, 3, dummy_participant());
        let creds = u.into_ice_creds(&mut os_rng());
        assert_eq!(creds.ufrag.len(), 40);
        assert_eq!(creds.pass.len(), 24); // 15 bytes → 24 Crockford chars
        assert!(creds.pass.len() >= 22);  // RFC 8445 minimum
    }
}
