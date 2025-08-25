use std::{fmt, hash, str::FromStr, sync::Arc};

use crate::rng::Rng;
use rand::RngCore;
use sha3::{Digest, Sha3_256};
use str0m::media::Mid;

pub type EntityId = String;

pub mod prefix {
    pub const API_KEY_ID: &str = "kid";
    pub const API_PUBLIC_KEY: &str = "pk";
    pub const API_SECRET: &str = "sk";
    pub const PROJECT_ID: &str = "p";
    pub const ROOM_ID: &str = "rm";
    pub const PARTICIPANT_ID: &str = "pa";
    pub const USER_ID: &str = "u";
    pub const TRACK_ID: &str = "tr";
}

const HASH_OUTPUT_BYTES: usize = 16;

/// Generates a new random entity ID with optional prefix and length (in bytes).
pub fn new_random_id(rng: &mut Rng, prefix: &str, length: usize) -> EntityId {
    let mut bytes = vec![0u8; length];
    rng.fill_bytes(&mut bytes);
    encode_with_prefix(prefix, &bytes)
}

pub fn new_entity_id(rng: &mut Rng, prefix: &str) -> EntityId {
    new_random_id(rng, prefix, HASH_OUTPUT_BYTES)
}

pub fn new_hashed_id(prefix: &str, input: &str) -> EntityId {
    let mut hasher = Sha3_256::default();
    hasher.update(input.as_bytes());
    let full_hash = hasher.finalize();
    let hash_output_slice = &full_hash[..HASH_OUTPUT_BYTES];

    encode_with_prefix(prefix, hash_output_slice)
}

/// Decodes the base58-encoded part of the ID into raw bytes.
pub fn decode_id(id: &str) -> Option<Vec<u8>> {
    id.split_once('_')
        .and_then(|(_, encoded)| bs58::decode(encoded).into_vec().ok())
}

/// Returns the prefix part (before `_`) of the ID.
pub fn get_prefix(id: &str) -> Option<&str> {
    id.split_once('_').map(|(prefix, _)| prefix)
}

/// Returns the base58 portion (after `_`) of the ID.
pub fn get_encoded_part(id: &str) -> Option<&str> {
    id.split_once('_').map(|(_, b58)| b58)
}

/// Returns the ID as a string reference (useful if passed as EntityId).
pub fn as_str(id: &EntityId) -> &str {
    id.as_str()
}

fn encode_with_prefix(prefix: &str, bytes: &[u8]) -> EntityId {
    let encoded = bs58::encode(bytes).into_string();
    format!("{}_{}", prefix, encoded)
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum IdValidationError {
    #[error("ID exceeds maximum length of {0} characters")]
    TooLong(usize),
    #[error("ID contains invalid characters")]
    InvalidCharacters,
    #[error("ID is empty")]
    Empty,
}

fn validate_id_string(s: &str, max_len: usize) -> Result<(), IdValidationError> {
    if s.is_empty() {
        return Err(IdValidationError::Empty);
    }
    if s.len() > max_len {
        return Err(IdValidationError::TooLong(max_len));
    }
    // Check if all characters are in the allowed set: 0-9, A-Z, a-z, _, -
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(IdValidationError::InvalidCharacters);
    }
    Ok(())
}

pub struct RoomId {
    pub external: ExternalRoomId,
    pub internal: EntityId,
}

impl RoomId {
    pub fn new(external: ExternalRoomId) -> Self {
        let internal = new_hashed_id(prefix::ROOM_ID, external.as_str());
        Self { external, internal }
    }
}

impl hash::Hash for RoomId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.internal.hash(state);
    }
}

impl Eq for RoomId {}

impl PartialEq for RoomId {
    fn eq(&self, other: &Self) -> bool {
        self.internal.eq(&other.internal)
    }
}

impl fmt::Display for RoomId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.internal, f)
    }
}

impl fmt::Debug for RoomId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.internal, f)
    }
}

impl AsRef<str> for RoomId {
    fn as_ref(&self) -> &str {
        &self.internal
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct ExternalRoomId(String);

impl ExternalRoomId {
    const MAX_LEN: usize = 20;

    pub fn new(id: String) -> Result<Self, IdValidationError> {
        validate_id_string(&id, Self::MAX_LEN)?;
        Ok(ExternalRoomId(id))
    }

    /// Returns a reference to the inner string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ExternalRoomId {
    type Err = IdValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ExternalRoomId::new(s.to_string())
    }
}

impl fmt::Display for ExternalRoomId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl TryFrom<String> for ExternalRoomId {
    type Error = IdValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        ExternalRoomId::new(value)
    }
}

impl AsRef<str> for ExternalRoomId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

pub struct ParticipantId {
    pub external: ExternalParticipantId,
    pub internal: EntityId,
}

impl ParticipantId {
    pub fn new(rng: &mut Rng, external: ExternalParticipantId) -> Self {
        let internal = new_entity_id(rng, prefix::PARTICIPANT_ID);
        Self { external, internal }
    }
}

impl hash::Hash for ParticipantId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.internal.hash(state);
    }
}

impl PartialOrd for ParticipantId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ParticipantId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.internal.cmp(&other.internal)
    }
}

impl Eq for ParticipantId {}

impl PartialEq for ParticipantId {
    fn eq(&self, other: &Self) -> bool {
        self.internal.eq(&other.internal)
    }
}

impl fmt::Display for ParticipantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.internal, f)
    }
}

impl fmt::Debug for ParticipantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.internal, f)
    }
}

impl AsRef<str> for ParticipantId {
    fn as_ref(&self) -> &str {
        &self.internal
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, PartialOrd, Ord,
)]
#[serde(try_from = "String")]
pub struct ExternalParticipantId(String);

impl ExternalParticipantId {
    const MAX_LEN: usize = 20;

    pub fn new(id: String) -> Result<Self, IdValidationError> {
        validate_id_string(&id, Self::MAX_LEN)?;
        Ok(ExternalParticipantId(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ExternalParticipantId {
    type Err = IdValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ExternalParticipantId::new(s.to_string())
    }
}

impl fmt::Display for ExternalParticipantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl TryFrom<String> for ExternalParticipantId {
    type Error = IdValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        ExternalParticipantId::new(value)
    }
}

impl AsRef<str> for ExternalParticipantId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

pub struct TrackId {
    pub internal: Arc<EntityId>,
    pub origin_participant: Arc<ParticipantId>,
    pub origin_mid: Mid,
}

impl TrackId {
    pub fn new(rng: &mut Rng, participant_id: Arc<ParticipantId>, mid: Mid) -> Self {
        let internal = new_entity_id(rng, prefix::TRACK_ID);
        Self {
            internal: Arc::new(internal),
            origin_participant: participant_id,
            origin_mid: mid,
        }
    }
}

impl hash::Hash for TrackId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.internal.hash(state);
    }
}

impl Eq for TrackId {}

impl PartialEq for TrackId {
    fn eq(&self, other: &Self) -> bool {
        self.internal.eq(&other.internal)
    }
}

impl fmt::Display for TrackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.internal, f)
    }
}

impl fmt::Debug for TrackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.internal, f)
    }
}

impl AsRef<str> for TrackId {
    fn as_ref(&self) -> &str {
        &self.internal
    }
}

#[cfg(test)]
mod tests {
    use rand::SeedableRng;

    use super::*;
    use std::hash::{Hash, Hasher};

    #[test]
    fn test_random_id_generation() {
        let mut rng = Rng::seed_from_u64(1);
        let id = new_random_id(&mut rng, prefix::USER_ID, 16);
        assert!(id.starts_with("u_"));
        let decoded = decode_id(&id).unwrap();
        assert_eq!(decoded.len(), 16);
    }

    #[test]
    fn test_hashed_id_generation() {
        let id = new_hashed_id(prefix::ROOM_ID, "hello world");
        assert!(id.starts_with("rm_"));
        let decoded = decode_id(&id).unwrap();
        assert_eq!(decoded.len(), HASH_OUTPUT_BYTES);
    }

    #[test]
    fn test_get_prefix_and_encoded() {
        let id = "pa_DEF123";
        assert_eq!(get_prefix(id), Some("pa"));
        assert_eq!(get_encoded_part(id), Some("DEF123"));
    }

    #[test]
    fn test_as_str() {
        let id: EntityId = "p_ABCDEFG".to_string();
        assert_eq!(as_str(&id), "p_ABCDEFG");
    }

    #[test]
    fn test_decode_id_invalid_format() {
        assert_eq!(decode_id("invalidid"), None);
        assert_eq!(decode_id("no_base58_"), None); // invalid base58
    }

    #[test]
    fn test_validate_id_string_success() {
        let valid = "abc123_-XYZ";
        assert!(validate_id_string(valid, 20).is_ok());
    }

    #[test]
    fn test_validate_id_string_errors() {
        assert_eq!(
            validate_id_string("", 10).unwrap_err(),
            IdValidationError::Empty
        );
        assert_eq!(
            validate_id_string("a".repeat(21).as_str(), 20).unwrap_err(),
            IdValidationError::TooLong(20)
        );
        assert_eq!(
            validate_id_string("abc$", 10).unwrap_err(),
            IdValidationError::InvalidCharacters
        );
    }

    #[test]
    fn test_external_room_id_new_and_display() {
        let id_str = "Room123_valid";
        let id = ExternalRoomId::new(id_str.to_string()).unwrap();
        assert_eq!(id.to_string(), id_str);
        assert_eq!(id.as_str(), id_str);
    }

    #[test]
    fn test_external_room_id_try_from() {
        let id: Result<ExternalRoomId, _> = "validRoom".to_string().try_into();
        assert!(id.is_ok());

        let too_long: Result<ExternalRoomId, _> = "x".repeat(21).try_into();
        assert!(matches!(
            too_long.unwrap_err(),
            IdValidationError::TooLong(20)
        ));
    }

    #[test]
    fn test_room_id_equality_and_hash() {
        let internal = "rm_abc123".to_string();
        let ext = ExternalRoomId::new("external".into()).unwrap();
        let id1 = RoomId {
            external: ext.clone(),
            internal: internal.clone(),
        };
        let id2 = RoomId {
            external: ext,
            internal,
        };

        assert_eq!(id1, id2);
        use std::collections::hash_map::DefaultHasher;
        let mut h1 = DefaultHasher::new();
        let mut h2 = DefaultHasher::new();
        id1.hash(&mut h1);
        id2.hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish());
    }

    #[test]
    fn test_external_participant_id() {
        let valid = "part123";
        let id = ExternalParticipantId::new(valid.to_string()).unwrap();
        assert_eq!(id.as_str(), valid);
        assert_eq!(id.to_string(), valid);
    }

    #[test]
    fn test_participant_id_equality_and_hash() {
        let internal = "pa_xyz987".to_string();
        let ext = ExternalParticipantId::new("external_pa".into()).unwrap();
        let id1 = ParticipantId {
            external: ext.clone(),
            internal: internal.clone(),
        };
        let id2 = ParticipantId {
            external: ext,
            internal,
        };

        assert_eq!(id1, id2);
        use std::collections::hash_map::DefaultHasher;
        let mut h1 = DefaultHasher::new();
        let mut h2 = DefaultHasher::new();
        id1.hash(&mut h1);
        id2.hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish());
    }
}
