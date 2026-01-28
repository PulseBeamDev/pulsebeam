use std::sync::Arc;
use std::{fmt, str::FromStr};

use derive_more::{AsRef, Display};
use pulsebeam_runtime::prelude::*;
use pulsebeam_runtime::rand;
use sha3::{Digest, Sha3_256};

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
pub fn new_random_id(prefix: &str, length: usize) -> EntityId {
    let mut bytes = vec![0u8; length];
    let mut rng = rand::rng();
    rng.fill_bytes(&mut bytes);
    encode_with_prefix(prefix, &bytes)
}

pub fn new_entity_id(prefix: &str) -> EntityId {
    new_random_id(prefix, HASH_OUTPUT_BYTES)
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

fn encode_with_prefix(prefix: &str, bytes: &[u8]) -> EntityId {
    let encoded = bs58::encode(bytes).into_string();
    format!("{}_{}", prefix, encoded)
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum IdValidationError {
    #[error("ID exceeds maximum length of {0} characters")]
    TooLong(usize),
    #[error("ID contains invalid characters")]
    InvalidCharacters,
    #[error("ID is empty")]
    Empty,
    #[error("Invalid prefix: expected {expected}, got {got}")]
    InvalidPrefix { expected: String, got: String },
}

pub fn validate_id_string(s: &str, max_len: usize) -> Result<(), IdValidationError> {
    if s.is_empty() {
        return Err(IdValidationError::Empty);
    }
    if s.len() > max_len {
        return Err(IdValidationError::TooLong(max_len));
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(IdValidationError::InvalidCharacters);
    }
    Ok(())
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RoomId {
    external: ExternalRoomId,
    internal: Arc<EntityId>,
}

impl RoomId {
    pub fn new(external: ExternalRoomId) -> Self {
        let internal = Arc::new(new_hashed_id(prefix::ROOM_ID, external.as_str()));
        Self { external, internal }
    }

    pub fn external(&self) -> &ExternalRoomId {
        &self.external
    }

    pub fn internal(&self) -> &str {
        &self.internal
    }

    pub fn as_str(&self) -> &str {
        &self.internal
    }
}

impl TryFrom<&str> for RoomId {
    type Error = IdValidationError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        let external = ExternalRoomId::from_str(value)?;
        Ok(Self::new(external))
    }
}

impl fmt::Display for RoomId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&*self.internal, f)
    }
}

impl fmt::Debug for RoomId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RoomId")
            .field("external", &self.external)
            .field("internal", &*self.internal)
            .finish()
    }
}

impl AsRef<str> for RoomId {
    fn as_ref(&self) -> &str {
        &self.internal
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, Display, AsRef, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String")]
#[as_ref(forward)]
pub struct ExternalRoomId(String);

impl ExternalRoomId {
    const MAX_LEN: usize = 20;

    pub fn new(id: String) -> Result<Self, IdValidationError> {
        validate_id_string(&id, Self::MAX_LEN)?;
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl FromStr for ExternalRoomId {
    type Err = IdValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s.to_string())
    }
}

impl TryFrom<String> for ExternalRoomId {
    type Error = IdValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct ParticipantId {
    internal: Arc<EntityId>,
}

impl ParticipantId {
    pub fn new() -> Self {
        Self {
            internal: Arc::new(new_entity_id(prefix::PARTICIPANT_ID)),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.internal
    }
}

impl Default for ParticipantId {
    fn default() -> Self {
        Self::new()
    }
}

impl TryFrom<String> for ParticipantId {
    type Error = IdValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let expected_prefix = prefix::PARTICIPANT_ID;

        if !value.starts_with(&format!("{}_", expected_prefix)) {
            let got = get_prefix(&value).unwrap_or("none").to_string();
            return Err(IdValidationError::InvalidPrefix {
                expected: expected_prefix.to_string(),
                got,
            });
        }

        let encoded = value
            .strip_prefix(&format!("{}_", expected_prefix))
            .ok_or(IdValidationError::InvalidCharacters)?;

        bs58::decode(encoded)
            .into_vec()
            .map_err(|_| IdValidationError::InvalidCharacters)?;

        Ok(Self {
            internal: Arc::new(value),
        })
    }
}

impl FromStr for ParticipantId {
    type Err = IdValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_from(s.to_string())
    }
}

impl fmt::Display for ParticipantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&*self.internal, f)
    }
}

impl fmt::Debug for ParticipantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ParticipantId")
            .field(&*self.internal)
            .finish()
    }
}

impl AsRef<str> for ParticipantId {
    fn as_ref(&self) -> &str {
        &self.internal
    }
}

#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Display,
    AsRef,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(try_from = "String")]
#[as_ref(forward)]
pub struct ExternalParticipantId(String);

impl ExternalParticipantId {
    const MAX_LEN: usize = 20;

    pub fn new(id: String) -> Result<Self, IdValidationError> {
        validate_id_string(&id, Self::MAX_LEN)?;
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl FromStr for ExternalParticipantId {
    type Err = IdValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s.to_string())
    }
}

impl TryFrom<String> for ExternalParticipantId {
    type Error = IdValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

#[derive(Clone, PartialOrd, Ord, Eq, PartialEq, Hash)]
pub struct TrackId {
    internal: Arc<EntityId>,
}

impl TrackId {
    pub fn new() -> Self {
        Self {
            internal: Arc::new(new_entity_id(prefix::TRACK_ID)),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.internal
    }
}

impl Default for TrackId {
    fn default() -> Self {
        Self::new()
    }
}

impl TryFrom<String> for TrackId {
    type Error = IdValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        let expected_prefix = prefix::TRACK_ID;

        if !value.starts_with(&format!("{}_", expected_prefix)) {
            let got = get_prefix(&value).unwrap_or("none").to_string();
            return Err(IdValidationError::InvalidPrefix {
                expected: expected_prefix.to_string(),
                got,
            });
        }

        let encoded = value
            .strip_prefix(&format!("{}_", expected_prefix))
            .ok_or(IdValidationError::InvalidCharacters)?;

        bs58::decode(encoded)
            .into_vec()
            .map_err(|_| IdValidationError::InvalidCharacters)?;

        Ok(Self {
            internal: Arc::new(value),
        })
    }
}

impl FromStr for TrackId {
    type Err = IdValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_from(s.to_string())
    }
}

impl fmt::Display for TrackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&*self.internal, f)
    }
}

impl fmt::Debug for TrackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("TrackId").field(&*self.internal).finish()
    }
}

impl AsRef<str> for TrackId {
    fn as_ref(&self) -> &str {
        &self.internal
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::{Hash, Hasher};

    #[test]
    fn test_random_id_generation() {
        let id = new_random_id(prefix::USER_ID, 16);
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
    fn test_decode_id_invalid_format() {
        assert_eq!(decode_id("invalidid"), None);
        assert_eq!(decode_id("no_base58_"), None);
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
        let ext = ExternalRoomId::new("external".into()).unwrap();
        let id1 = RoomId::new(ext.clone());
        let id2 = RoomId::new(ext);

        assert_eq!(id1, id2);

        use std::collections::hash_map::DefaultHasher;
        let mut h1 = DefaultHasher::new();
        let mut h2 = DefaultHasher::new();
        id1.hash(&mut h1);
        id2.hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish());
    }

    #[test]
    fn test_room_id_clone_is_cheap() {
        let ext = ExternalRoomId::new("test".into()).unwrap();
        let id1 = RoomId::new(ext);
        let id2 = id1.clone();

        // Verify they share the same Arc
        assert!(Arc::ptr_eq(&id1.internal, &id2.internal));
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
        let id1 = ParticipantId::new();
        let id2 = id1.clone();

        // Clones should share same Arc
        assert!(Arc::ptr_eq(&id1.internal, &id2.internal));

        use std::collections::hash_map::DefaultHasher;
        let mut h1 = DefaultHasher::new();
        let mut h2 = DefaultHasher::new();
        id1.hash(&mut h1);
        id2.hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish());
    }

    #[test]
    fn test_participant_id_try_from() {
        let id = ParticipantId::new();
        let id_str = id.to_string();

        let parsed: ParticipantId = id_str.parse().unwrap();
        assert_eq!(parsed.as_str(), id.as_str());
    }

    #[test]
    fn test_participant_id_invalid_prefix() {
        let result = ParticipantId::try_from("rm_123456".to_string());
        assert!(matches!(
            result.unwrap_err(),
            IdValidationError::InvalidPrefix { .. }
        ));
    }

    #[test]
    fn test_track_id_new_and_clone() {
        let id1 = TrackId::new();
        let id2 = id1.clone();

        // Verify they share the same Arc
        assert!(Arc::ptr_eq(&id1.internal, &id2.internal));
    }

    #[test]
    fn test_track_id_try_from() {
        let id = TrackId::new();
        let id_str = id.to_string();

        let parsed: TrackId = id_str.parse().unwrap();
        assert_eq!(parsed.as_str(), id.as_str());
    }

    #[test]
    fn test_all_ids_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}

        assert_send_sync::<RoomId>();
        assert_send_sync::<ParticipantId>();
        assert_send_sync::<TrackId>();
        assert_send_sync::<ExternalRoomId>();
        assert_send_sync::<ExternalParticipantId>();
    }
}
