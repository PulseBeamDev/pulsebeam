use derive_more::{AsRef, Display};
use pulsebeam_runtime::prelude::*;
use pulsebeam_runtime::rand;
use sha3::{Digest, Sha3_256};
use std::hash::Hasher;
use std::sync::Arc;
use std::{fmt, str::FromStr};

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
const MAX_INTERNAL_ID_LEN: usize = 20;
const MAX_EXTERNAL_ID_LEN: usize = 36;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum IdValidationError {
    #[error("ID exceeds maximum length")]
    TooLong(usize),
    #[error("ID contains invalid characters")]
    InvalidCharacters,
    #[error("ID is empty")]
    Empty,
    #[error("Invalid prefix: expected {expected}, got {got}")]
    InvalidPrefix { expected: String, got: String },
    #[error("Invalid encoding")]
    InvalidEncoding,
}

fn encode_with_prefix(prefix: &str, bytes: &[u8]) -> EntityId {
    let encoded = bs58::encode(bytes).into_string();
    format!("{}_{}", prefix, encoded)
}

pub fn new_random_id(prefix: &str, length: usize) -> EntityId {
    let mut bytes = vec![0u8; length];
    rand::rng().fill_bytes(&mut bytes);
    encode_with_prefix(prefix, &bytes)
}

pub fn new_hashed_id(prefix: &str, input: &str) -> EntityId {
    let mut hasher = Sha3_256::default();
    hasher.update(input.as_bytes());
    let full_hash = hasher.finalize();
    encode_with_prefix(prefix, &full_hash[..HASH_OUTPUT_BYTES])
}

pub fn validate_external_string(s: &str) -> Result<(), IdValidationError> {
    if s.is_empty() {
        return Err(IdValidationError::Empty);
    }
    if s.len() > MAX_EXTERNAL_ID_LEN {
        return Err(IdValidationError::TooLong(MAX_EXTERNAL_ID_LEN));
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(IdValidationError::InvalidCharacters);
    }
    Ok(())
}

fn validate_internal_format(value: &str, expected_prefix: &str) -> Result<(), IdValidationError> {
    if value.len() > MAX_INTERNAL_ID_LEN {
        return Err(IdValidationError::TooLong(MAX_INTERNAL_ID_LEN));
    }
    let Some((prefix, encoded)) = value.split_once('_') else {
        return Err(IdValidationError::InvalidEncoding);
    };
    if prefix != expected_prefix {
        return Err(IdValidationError::InvalidPrefix {
            expected: expected_prefix.to_string(),
            got: prefix.to_string(),
        });
    }
    bs58::decode(encoded)
        .into_vec()
        .map_err(|_| IdValidationError::InvalidEncoding)?;
    Ok(())
}

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, Display, AsRef, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String")]
#[as_ref(forward)]
pub struct ExternalRoomId(String);

impl ExternalRoomId {
    pub fn new(id: String) -> Result<Self, IdValidationError> {
        validate_external_string(&id)?;
        Ok(Self(id))
    }
    pub fn as_str(&self) -> &str {
        &self.0
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

#[derive(Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct RoomId {
    external: ExternalRoomId,
    internal: Arc<EntityId>,
}

impl RoomId {
    pub fn from_external(external: ExternalRoomId) -> Self {
        let internal = new_hashed_id(prefix::ROOM_ID, external.as_str());
        Self {
            external,
            internal: Arc::new(internal),
        }
    }

    pub fn external(&self) -> &ExternalRoomId {
        &self.external
    }
    pub fn internal(&self) -> &str {
        &self.internal
    }
}

impl std::hash::Hash for RoomId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.external.hash(state);
    }
}

impl FromStr for RoomId {
    type Err = IdValidationError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let external = ExternalRoomId::from_str(s)?;
        Ok(Self::from_external(external))
    }
}

impl TryFrom<String> for RoomId {
    type Error = IdValidationError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::from_str(&value)
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

#[derive(
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Display,
    AsRef,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(try_from = "String")]
#[as_ref(forward)]
pub struct ParticipantId {
    #[display(fmt = "{}", "_0")]
    internal: Arc<EntityId>,
}

impl ParticipantId {
    pub fn new() -> Self {
        Self {
            internal: Arc::new(new_random_id(prefix::PARTICIPANT_ID, HASH_OUTPUT_BYTES)),
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
        validate_internal_format(&value, prefix::PARTICIPANT_ID)?;
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

impl fmt::Debug for ParticipantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ParticipantId")
            .field(&*self.internal)
            .finish()
    }
}

#[derive(
    Clone,
    PartialOrd,
    Ord,
    Eq,
    PartialEq,
    Hash,
    Display,
    AsRef,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(try_from = "String")]
#[as_ref(forward)]
pub struct TrackId {
    #[display(fmt = "{}", "_0")]
    internal: Arc<EntityId>,
}

impl TrackId {
    pub fn new() -> Self {
        Self {
            internal: Arc::new(new_random_id(prefix::TRACK_ID, HASH_OUTPUT_BYTES)),
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
        validate_internal_format(&value, prefix::TRACK_ID)?;
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

impl fmt::Debug for TrackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("TrackId").field(&*self.internal).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_room_id_from_external_input() {
        let input = "my-room";
        let room_id = RoomId::from_str(input).unwrap();
        assert!(room_id.to_string().starts_with("rm_"));
        assert!(room_id.internal().starts_with("rm_"));
    }

    #[test]
    fn test_internal_validation() {
        let id = ParticipantId::new();
        let parsed = ParticipantId::from_str(&id.to_string());
        assert!(parsed.is_ok());
    }
}
