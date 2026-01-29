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
const MAX_INTERNAL_ID_LEN: usize = 36;
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
    if encoded.is_empty() {
        return Err(IdValidationError::InvalidEncoding);
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

#[derive(Clone, PartialEq, Eq)]
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

impl serde::Serialize for RoomId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.external.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for RoomId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let external = ExternalRoomId::deserialize(deserializer)?;
        Ok(Self::from_external(external))
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

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Display, AsRef)]
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

impl serde::Serialize for ParticipantId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for ParticipantId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::try_from(s).map_err(serde::de::Error::custom)
    }
}

impl fmt::Debug for ParticipantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ParticipantId")
            .field(&*self.internal)
            .finish()
    }
}

#[derive(Clone, PartialOrd, Ord, Eq, PartialEq, Hash, Display, AsRef)]
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

impl serde::Serialize for TrackId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for TrackId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::try_from(s).map_err(serde::de::Error::custom)
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
    use std::collections::{HashMap, HashSet};

    #[test]
    fn room_id_from_external() {
        let room_id = RoomId::from_str("my-room").unwrap();
        assert!(room_id.to_string().starts_with("rm_"));
        assert!(room_id.internal().starts_with("rm_"));
    }

    #[test]
    fn participant_id_roundtrip() {
        let id = ParticipantId::new();
        let parsed = ParticipantId::from_str(&id.to_string()).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn external_room_id_rejects_sql_injection() {
        assert!(ExternalRoomId::new("room'; DROP TABLE users--".to_string()).is_err());
    }

    #[test]
    fn external_room_id_rejects_xss() {
        assert!(ExternalRoomId::new("<script>alert('xss')</script>".to_string()).is_err());
    }

    #[test]
    fn external_room_id_rejects_path_traversal() {
        assert!(ExternalRoomId::new("../../../etc/passwd".to_string()).is_err());
    }

    #[test]
    fn external_room_id_rejects_null_bytes() {
        assert!(ExternalRoomId::new("room\0id".to_string()).is_err());
    }

    #[test]
    fn external_room_id_rejects_unicode_exploits() {
        assert!(ExternalRoomId::new("room\u{202e}attacked".to_string()).is_err());
    }

    #[test]
    fn external_room_id_rejects_whitespace() {
        assert!(ExternalRoomId::new("room id".to_string()).is_err());
        assert!(ExternalRoomId::new("room\tid".to_string()).is_err());
        assert!(ExternalRoomId::new("room\nid".to_string()).is_err());
    }

    #[test]
    fn external_room_id_rejects_special_chars() {
        for input in ["room@id", "room#id", "room$id", "room%id", "room&id"] {
            assert!(ExternalRoomId::new(input.to_string()).is_err());
        }
    }

    #[test]
    fn external_room_id_empty_string() {
        assert_eq!(
            ExternalRoomId::new("".to_string()).unwrap_err(),
            IdValidationError::Empty
        );
    }

    #[test]
    fn external_room_id_too_long() {
        let too_long = "a".repeat(MAX_EXTERNAL_ID_LEN + 1);
        assert_eq!(
            ExternalRoomId::new(too_long).unwrap_err(),
            IdValidationError::TooLong(MAX_EXTERNAL_ID_LEN)
        );
    }

    #[test]
    fn external_room_id_max_length_accepted() {
        assert!(ExternalRoomId::new("a".repeat(MAX_EXTERNAL_ID_LEN)).is_ok());
    }

    #[test]
    fn external_room_id_accepts_valid_chars() {
        for input in [
            "room123", "ROOM123", "room_123", "room-123", "a", "Z", "0", "_", "-",
        ] {
            assert!(ExternalRoomId::new(input.to_string()).is_ok());
        }
    }

    #[test]
    fn participant_id_wrong_prefix() {
        let result = ParticipantId::from_str("rm_abc123def");
        assert!(matches!(
            result.unwrap_err(),
            IdValidationError::InvalidPrefix { .. }
        ));
    }

    #[test]
    fn track_id_wrong_prefix() {
        let result = TrackId::from_str("pa_abc123def");
        assert!(matches!(
            result.unwrap_err(),
            IdValidationError::InvalidPrefix { .. }
        ));
    }

    #[test]
    fn participant_id_invalid_base58() {
        let result = ParticipantId::from_str("pa_000OIl");
        assert!(matches!(
            result.unwrap_err(),
            IdValidationError::InvalidEncoding
        ));
    }

    #[test]
    fn participant_id_missing_separator() {
        let result = ParticipantId::from_str("paabc123def");
        assert!(matches!(
            result.unwrap_err(),
            IdValidationError::InvalidEncoding
        ));
    }

    #[test]
    fn participant_id_too_long() {
        let too_long = format!("pa_{}", "a".repeat(MAX_INTERNAL_ID_LEN));
        assert!(matches!(
            ParticipantId::from_str(&too_long).unwrap_err(),
            IdValidationError::TooLong(_)
        ));
    }

    #[test]
    fn track_id_empty_encoded_part() {
        assert!(TrackId::from_str("tr_").is_err());
    }

    #[test]
    fn room_id_deterministic_hashing() {
        let external = ExternalRoomId::new("test-room".to_string()).unwrap();
        let room1 = RoomId::from_external(external.clone());
        let room2 = RoomId::from_external(external);
        assert_eq!(room1.internal(), room2.internal());
        assert_eq!(room1, room2);
    }

    #[test]
    fn room_id_different_external_different_hash() {
        let ext1 = ExternalRoomId::new("room1".to_string()).unwrap();
        let ext2 = ExternalRoomId::new("room2".to_string()).unwrap();
        let room1 = RoomId::from_external(ext1);
        let room2 = RoomId::from_external(ext2);
        assert_ne!(room1.internal(), room2.internal());
        assert_ne!(room1, room2);
    }

    #[test]
    fn participant_id_uniqueness() {
        let mut seen = HashSet::new();
        for _ in 0..1000 {
            assert!(seen.insert(ParticipantId::new().to_string()));
        }
    }

    #[test]
    fn track_id_uniqueness() {
        let mut seen = HashSet::new();
        for _ in 0..1000 {
            assert!(seen.insert(TrackId::new().to_string()));
        }
    }

    #[test]
    fn participant_id_serde_roundtrip() {
        let id = ParticipantId::new();
        let serialized = serde_json::to_string(&id).unwrap();
        let deserialized: ParticipantId = serde_json::from_str(&serialized).unwrap();
        assert_eq!(id, deserialized);
    }

    #[test]
    fn track_id_serde_roundtrip() {
        let id = TrackId::new();
        let serialized = serde_json::to_string(&id).unwrap();
        let deserialized: TrackId = serde_json::from_str(&serialized).unwrap();
        assert_eq!(id, deserialized);
    }

    #[test]
    fn room_id_serde_roundtrip() {
        let external = ExternalRoomId::new("test-room".to_string()).unwrap();
        let id = RoomId::from_external(external);
        let serialized = serde_json::to_string(&id).unwrap();
        let deserialized: RoomId = serde_json::from_str(&serialized).unwrap();
        assert_eq!(id.external(), deserialized.external());
    }

    #[test]
    fn external_room_id_serde_roundtrip() {
        let id = ExternalRoomId::new("test-room".to_string()).unwrap();
        let serialized = serde_json::to_string(&id).unwrap();
        let deserialized: ExternalRoomId = serde_json::from_str(&serialized).unwrap();
        assert_eq!(id, deserialized);
    }

    #[test]
    fn serde_rejects_invalid_participant_id() {
        let result: Result<ParticipantId, _> = serde_json::from_str(r#""rm_wrongprefix""#);
        assert!(result.is_err());
    }

    #[test]
    fn room_id_as_hashmap_key() {
        let mut map: HashMap<RoomId, String> = HashMap::new();
        let ext = ExternalRoomId::new("room1".to_string()).unwrap();
        let room_id = RoomId::from_external(ext);
        map.insert(room_id.clone(), "value".to_string());
        assert_eq!(map.get(&room_id), Some(&"value".to_string()));
    }

    #[test]
    fn participant_id_as_hashmap_key() {
        let mut map: HashMap<ParticipantId, String> = HashMap::new();
        let id = ParticipantId::new();
        map.insert(id.clone(), "value".to_string());
        assert_eq!(map.get(&id), Some(&"value".to_string()));
    }

    #[test]
    fn track_id_as_hashmap_key() {
        let mut map: HashMap<TrackId, String> = HashMap::new();
        let id = TrackId::new();
        map.insert(id.clone(), "value".to_string());
        assert_eq!(map.get(&id), Some(&"value".to_string()));
    }

    #[test]
    fn room_id_hash_consistency_after_clone() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::Hash;
        let ext = ExternalRoomId::new("room1".to_string()).unwrap();
        let room1 = RoomId::from_external(ext);
        let room2 = room1.clone();
        let mut h1 = DefaultHasher::new();
        let mut h2 = DefaultHasher::new();
        room1.hash(&mut h1);
        room2.hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish());
    }

    #[test]
    fn participant_id_arc_sharing() {
        let id1 = ParticipantId::new();
        let id2 = id1.clone();
        assert!(Arc::ptr_eq(&id1.internal, &id2.internal));
    }

    #[test]
    fn track_id_arc_sharing() {
        let id1 = TrackId::new();
        let id2 = id1.clone();
        assert!(Arc::ptr_eq(&id1.internal, &id2.internal));
    }

    #[test]
    fn room_id_arc_sharing() {
        let ext = ExternalRoomId::new("room1".to_string()).unwrap();
        let id1 = RoomId::from_external(ext);
        let id2 = id1.clone();
        assert!(Arc::ptr_eq(&id1.internal, &id2.internal));
    }

    #[test]
    fn all_ids_are_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<RoomId>();
        assert_send_sync::<ParticipantId>();
        assert_send_sync::<TrackId>();
        assert_send_sync::<ExternalRoomId>();
    }

    #[test]
    fn external_room_id_single_char() {
        assert!(ExternalRoomId::new("a".to_string()).is_ok());
        assert!(ExternalRoomId::new("Z".to_string()).is_ok());
        assert!(ExternalRoomId::new("0".to_string()).is_ok());
        assert!(ExternalRoomId::new("_".to_string()).is_ok());
        assert!(ExternalRoomId::new("-".to_string()).is_ok());
    }

    #[test]
    fn room_id_case_sensitivity() {
        let ext1 = ExternalRoomId::new("Room".to_string()).unwrap();
        let ext2 = ExternalRoomId::new("room".to_string()).unwrap();
        let room1 = RoomId::from_external(ext1);
        let room2 = RoomId::from_external(ext2);
        assert_ne!(room1.internal(), room2.internal());
    }

    #[test]
    fn display_format() {
        assert!(format!("{}", ParticipantId::new()).starts_with("pa_"));
        assert!(format!("{}", TrackId::new()).starts_with("tr_"));
        let ext = ExternalRoomId::new("test".to_string()).unwrap();
        assert!(format!("{}", RoomId::from_external(ext)).starts_with("rm_"));
    }

    #[test]
    fn debug_format() {
        assert!(format!("{:?}", ParticipantId::new()).contains("ParticipantId"));
        assert!(format!("{:?}", TrackId::new()).contains("TrackId"));
    }

    #[test]
    fn as_str_returns_valid_reference() {
        let participant_id = ParticipantId::new();
        assert!(participant_id.as_str().starts_with("pa_"));
        assert_eq!(participant_id.as_str(), participant_id.to_string());
        let track_id = TrackId::new();
        assert!(track_id.as_str().starts_with("tr_"));
        assert_eq!(track_id.as_str(), track_id.to_string());
    }

    #[test]
    fn webrtc_sfu_message_passing() {
        let room_ext = ExternalRoomId::new("conference-1".to_string()).unwrap();
        let room_id = RoomId::from_external(room_ext);
        let participant_id = ParticipantId::new();
        let track_id = TrackId::new();
        let room_clone = room_id.clone();
        let participant_clone = participant_id.clone();
        let track_clone = track_id.clone();
        assert_eq!(room_id, room_clone);
        assert_eq!(participant_id, participant_clone);
        assert_eq!(track_id, track_clone);
        assert!(Arc::ptr_eq(&room_id.internal, &room_clone.internal));
        assert!(Arc::ptr_eq(
            &participant_id.internal,
            &participant_clone.internal
        ));
        assert!(Arc::ptr_eq(&track_id.internal, &track_clone.internal));
    }

    #[test]
    fn parsing_from_client_input() {
        assert!(RoomId::from_str("my-conference-room").is_ok());
        let participant_id = ParticipantId::new();
        let serialized = participant_id.to_string();
        let parsed = ParticipantId::from_str(&serialized).unwrap();
        assert_eq!(parsed, participant_id);
    }

    #[test]
    fn storing_in_multiple_collections() {
        let participant_id = ParticipantId::new();
        let track_id = TrackId::new();
        let mut participant_map: HashMap<ParticipantId, Vec<TrackId>> = HashMap::new();
        let mut track_map: HashMap<TrackId, ParticipantId> = HashMap::new();
        participant_map.insert(participant_id.clone(), vec![track_id.clone()]);
        track_map.insert(track_id.clone(), participant_id.clone());
        assert_eq!(
            participant_map.get(&participant_id),
            Some(&vec![track_id.clone()])
        );
        assert_eq!(track_map.get(&track_id), Some(&participant_id));
    }

    #[test]
    fn comparison_and_ordering() {
        let id1 = ParticipantId::new();
        let id2 = ParticipantId::new();
        assert_ne!(id1, id2);
        let mut ids = [id1.clone(), id2.clone()];
        ids.sort();
        assert_eq!(id1, id1.clone());
    }

    #[test]
    fn collision_resistance() {
        let mut internal_ids = HashSet::new();
        for external in ["room", "room1", "Room", "ROOM", "room_", "room-1"] {
            let ext = ExternalRoomId::new(external.to_string()).unwrap();
            let room_id = RoomId::from_external(ext);
            assert!(internal_ids.insert(room_id.internal().to_string()));
        }
    }

    #[test]
    fn ids_are_url_safe() {
        for c in ParticipantId::new().as_str().chars() {
            assert!(c.is_ascii_alphanumeric() || c == '_');
        }
        for c in TrackId::new().as_str().chars() {
            assert!(c.is_ascii_alphanumeric() || c == '_');
        }
    }
}
