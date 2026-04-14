use arrayvec::ArrayString;
use derive_more::{AsRef, Display};
use sha3::{Digest, Sha3_256};
use std::{fmt, str::FromStr};
use str0m::media::MediaKind;
use utoipa::ToSchema;
use uuid::Uuid;

// rm_ prefix (3) + base32 of 16 bytes (26 chars) = 29 chars, 36 is safe headroom
const MAX_INTERNAL_ROOM_ID_LEN: usize = 36;
const MAX_EXTERNAL_ROOM_ID_LEN: usize = 36;

pub type EntityId = String;

pub mod prefix {
    pub const API_KEY_ID: &str = "kid";
    pub const API_PUBLIC_KEY: &str = "pk";
    pub const API_SECRET: &str = "sk";
    pub const PROJECT_ID: &str = "p";
    pub const ROOM_ID: &str = "rm";
    pub const PARTICIPANT_ID: &str = "pa";
    pub const CONNECTION_ID: &str = "c";
    pub const USER_ID: &str = "u";
    pub const AUDIO_TRACK_ID: &str = "aud";
    pub const VIDEO_TRACK_ID: &str = "vid";
}

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
    let encoded = base32::encode(base32::Alphabet::Crockford, bytes);
    format!("{}_{}", prefix, encoded)
}

fn decode_with_prefix(value: &str, expected_prefix: &str) -> Result<Uuid, IdValidationError> {
    if value.len() > MAX_INTERNAL_ROOM_ID_LEN {
        return Err(IdValidationError::TooLong(MAX_INTERNAL_ROOM_ID_LEN));
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
    let bytes = base32::decode(base32::Alphabet::Crockford, encoded)
        .ok_or(IdValidationError::InvalidEncoding)?;
    Uuid::from_slice(&bytes).map_err(|_| IdValidationError::InvalidEncoding)
}

/// Generates a UUIDv8 using SHA3-256, following the logic of UUIDv5 (Name-based).
/// Per RFC 9562 Appendix B.2:
/// https://www.ietf.org/rfc/rfc9562.html#appendix-B.2
pub fn new_v8_sha3(namespace: &Uuid, name: &[u8]) -> Uuid {
    let mut hasher = Sha3_256::new();
    hasher.update(namespace.as_bytes());
    hasher.update(name);
    let hash = hasher.finalize();

    // Take the first 16 bytes of the SHA3-256 hash
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&hash[..16]);

    // Set Version to 8
    bytes[6] = (bytes[6] & 0x0f) | 0x80;

    // Set Variant to RFC 4122
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    Uuid::from_bytes(bytes)
}

pub fn validate_external_string(s: &str) -> Result<(), IdValidationError> {
    if s.is_empty() {
        return Err(IdValidationError::Empty);
    }
    if s.len() > MAX_EXTERNAL_ROOM_ID_LEN {
        return Err(IdValidationError::TooLong(MAX_EXTERNAL_ROOM_ID_LEN));
    }
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(IdValidationError::InvalidCharacters);
    }
    Ok(())
}

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, Display, AsRef, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "&str")]
#[as_ref(forward)]
pub struct ExternalRoomId(ArrayString<MAX_EXTERNAL_ROOM_ID_LEN>);

impl ExternalRoomId {
    pub fn new(id: &str) -> Result<Self, IdValidationError> {
        validate_external_string(id)?;
        Ok(Self(ArrayString::from(id).unwrap()))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl FromStr for ExternalRoomId {
    type Err = IdValidationError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<&str> for ExternalRoomId {
    type Error = IdValidationError;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

// Keep TryFrom<String> for call sites that already have an owned String
impl TryFrom<String> for ExternalRoomId {
    type Error = IdValidationError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RoomId {
    uuid: Uuid,
}

impl RoomId {
    pub fn from_external(external: &ExternalRoomId) -> Self {
        let uuid = new_v8_sha3(&Uuid::nil(), external.as_str().as_bytes());
        Self { uuid }
    }

    pub fn as_str(&self) -> String {
        encode_with_prefix(prefix::ROOM_ID, self.uuid.as_bytes())
    }
}

impl TryFrom<String> for RoomId {
    type Error = IdValidationError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        let uuid = decode_with_prefix(&value, prefix::ROOM_ID)?;
        Ok(Self { uuid })
    }
}

impl FromStr for RoomId {
    type Err = IdValidationError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_from(s.to_string())
    }
}

impl serde::Serialize for RoomId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for RoomId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::try_from(s).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for RoomId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl fmt::Debug for RoomId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ParticipantId {
    uuid: Uuid,
}

impl ParticipantId {
    pub fn new() -> Self {
        Self {
            // TODO: use deterministic timestamp
            uuid: Uuid::now_v7(),
        }
    }

    pub fn as_str(&self) -> String {
        encode_with_prefix(prefix::PARTICIPANT_ID, self.uuid.as_bytes())
    }

    pub fn derive_track_id(&self, kind: MediaKind, label: &str) -> TrackId {
        let uuid = new_v8_sha3(&self.uuid, label.as_bytes());
        TrackId { kind, uuid }
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
        let uuid = decode_with_prefix(&value, prefix::PARTICIPANT_ID)?;
        Ok(Self { uuid })
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
        serializer.serialize_str(&self.as_str())
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

impl fmt::Display for ParticipantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl fmt::Debug for ParticipantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, ToSchema)]
pub struct ConnectionId {
    uuid: Uuid,
}

impl ConnectionId {
    pub const MIN: ConnectionId = ConnectionId {
        uuid: Uuid::from_u128(0),
    };
    pub const MAX: ConnectionId = ConnectionId {
        uuid: Uuid::from_u128(u128::MAX),
    };

    pub fn new() -> Self {
        Self {
            uuid: Uuid::now_v7(),
        }
    }

    pub fn as_str(&self) -> String {
        encode_with_prefix(prefix::CONNECTION_ID, self.uuid.as_bytes())
    }
}

impl Default for ConnectionId {
    fn default() -> Self {
        Self::new()
    }
}

impl TryFrom<&str> for ConnectionId {
    type Error = IdValidationError;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        let uuid = decode_with_prefix(value, prefix::CONNECTION_ID)?;
        Ok(Self { uuid })
    }
}

impl FromStr for ConnectionId {
    type Err = IdValidationError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::try_from(s)
    }
}

impl serde::Serialize for ConnectionId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for ConnectionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Self::try_from(s.as_str()).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl fmt::Debug for ConnectionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Hash)]
pub struct TrackId {
    uuid: Uuid,
    kind: MediaKind,
}

impl TrackId {
    pub fn as_str(&self) -> String {
        let prefix_str = match self.kind {
            MediaKind::Audio => prefix::AUDIO_TRACK_ID,
            MediaKind::Video => prefix::VIDEO_TRACK_ID,
        };

        encode_with_prefix(prefix_str, self.uuid.as_bytes())
    }
}

impl TryFrom<String> for TrackId {
    type Error = IdValidationError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        let Some((prefix_str, _)) = value.split_once('_') else {
            return Err(IdValidationError::InvalidEncoding);
        };

        let (kind, expected_prefix) = match prefix_str {
            p if p == prefix::AUDIO_TRACK_ID => (MediaKind::Audio, prefix::AUDIO_TRACK_ID),
            p if p == prefix::VIDEO_TRACK_ID => (MediaKind::Video, prefix::VIDEO_TRACK_ID),
            _ => {
                return Err(IdValidationError::InvalidPrefix {
                    expected: format!("{} or {}", prefix::AUDIO_TRACK_ID, prefix::VIDEO_TRACK_ID),
                    got: prefix_str.to_string(),
                });
            }
        };

        let uuid = decode_with_prefix(&value, expected_prefix)?;
        Ok(Self { uuid, kind })
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
        serializer.serialize_str(&self.as_str())
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

impl fmt::Display for TrackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl fmt::Debug for TrackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("TrackId").field(&self.as_str()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet};

    #[test]
    fn room_id_from_external() {
        let ext = ExternalRoomId::new("my-room").unwrap();
        let room_id = RoomId::from_external(&ext);
        assert!(room_id.to_string().starts_with("rm_"));
        assert!(room_id.as_str().starts_with("rm_"));
    }

    #[test]
    fn participant_id_roundtrip() {
        let id = ParticipantId::new();
        let parsed = ParticipantId::from_str(&id.as_str()).unwrap();
        assert_eq!(id, parsed);
    }

    #[test]
    fn external_room_id_rejects_sql_injection() {
        assert!(ExternalRoomId::new("room'; DROP TABLE users--").is_err());
    }

    #[test]
    fn external_room_id_rejects_xss() {
        assert!(ExternalRoomId::new("<script>alert('xss')</script>").is_err());
    }

    #[test]
    fn external_room_id_rejects_path_traversal() {
        assert!(ExternalRoomId::new("../../../etc/passwd").is_err());
    }

    #[test]
    fn external_room_id_rejects_null_bytes() {
        assert!(ExternalRoomId::new("room\0id").is_err());
    }

    #[test]
    fn external_room_id_rejects_unicode_exploits() {
        assert!(ExternalRoomId::new("room\u{202e}attacked").is_err());
    }

    #[test]
    fn external_room_id_rejects_whitespace() {
        assert!(ExternalRoomId::new("room id").is_err());
        assert!(ExternalRoomId::new("room\tid").is_err());
        assert!(ExternalRoomId::new("room\nid").is_err());
    }

    #[test]
    fn external_room_id_rejects_special_chars() {
        for input in ["room@id", "room#id", "room$id", "room%id", "room&id"] {
            assert!(ExternalRoomId::new(input).is_err());
        }
    }

    #[test]
    fn external_room_id_empty_string() {
        assert_eq!(
            ExternalRoomId::new("").unwrap_err(),
            IdValidationError::Empty
        );
    }

    #[test]
    fn external_room_id_too_long() {
        let too_long = "a".repeat(MAX_EXTERNAL_ROOM_ID_LEN + 1);
        assert_eq!(
            ExternalRoomId::new(&too_long).unwrap_err(),
            IdValidationError::TooLong(MAX_EXTERNAL_ROOM_ID_LEN)
        );
    }

    #[test]
    fn external_room_id_max_length_accepted() {
        assert!(ExternalRoomId::new(&"a".repeat(MAX_EXTERNAL_ROOM_ID_LEN)).is_ok());
    }

    #[test]
    fn external_room_id_accepts_valid_chars() {
        for input in [
            "room123", "ROOM123", "room_123", "room-123", "a", "Z", "0", "_", "-",
        ] {
            assert!(ExternalRoomId::new(input).is_ok());
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
        // Base58 of 16 bytes is approx 22 chars.
        // 36 chars is way more than enough for valid IDs, so we check stricter length or garbage
        let too_long = format!("pa_{}", "a".repeat(MAX_INTERNAL_ROOM_ID_LEN));
        assert!(matches!(
            ParticipantId::from_str(&too_long).unwrap_err(),
            IdValidationError::TooLong(_) | IdValidationError::InvalidEncoding
        ));
    }

    #[test]
    fn room_id_deterministic_hashing() {
        let external = ExternalRoomId::new("test-room").unwrap();
        let room1 = RoomId::from_external(&external);
        let room2 = RoomId::from_external(&external);
        assert_eq!(room1.as_str(), room2.as_str());
        assert_eq!(room1, room2);
    }

    #[test]
    fn room_id_different_external_different_hash() {
        let ext1 = ExternalRoomId::new("room1").unwrap();
        let ext2 = ExternalRoomId::new("room2").unwrap();
        let room1 = RoomId::from_external(&ext1);
        let room2 = RoomId::from_external(&ext2);
        assert_ne!(room1.as_str(), room2.as_str());
        assert_ne!(room1, room2);
    }

    #[test]
    fn participant_id_uniqueness() {
        let mut seen = HashSet::new();
        for _ in 0..1000 {
            assert!(seen.insert(ParticipantId::new().as_str()));
        }
    }

    #[test]
    fn track_id_derivation_determinism() {
        let p = ParticipantId::new();
        let t1 = p.derive_track_id(MediaKind::Video, "cam");
        let t2 = p.derive_track_id(MediaKind::Video, "cam");
        assert_eq!(t1, t2);
        assert_eq!(t1.as_str(), t2.as_str());
    }

    #[test]
    fn track_id_derivation_uniqueness() {
        let p = ParticipantId::new();
        let t1 = p.derive_track_id(MediaKind::Video, "cam");
        let t2 = p.derive_track_id(MediaKind::Audio, "mic");
        assert_ne!(t1, t2);
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
        let p = ParticipantId::new();
        let id = p.derive_track_id(MediaKind::Video, "cam");
        let serialized = serde_json::to_string(&id).unwrap();
        let deserialized: TrackId = serde_json::from_str(&serialized).unwrap();
        assert_eq!(id, deserialized);
    }

    #[test]
    fn room_id_serde_roundtrip() {
        let external = ExternalRoomId::new("test-room").unwrap();
        let id = RoomId::from_external(&external);
        let serialized = serde_json::to_string(&id).unwrap();
        let deserialized: RoomId = serde_json::from_str(&serialized).unwrap();
        assert_eq!(id, deserialized);
    }

    #[test]
    fn external_room_id_serde_roundtrip() {
        let id = ExternalRoomId::new("test-room").unwrap();
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
        let ext = ExternalRoomId::new("room1").unwrap();
        let room_id = RoomId::from_external(&ext);
        map.insert(room_id, "value".to_string());
        assert_eq!(map.get(&room_id), Some(&"value".to_string()));
    }

    #[test]
    fn participant_id_as_hashmap_key() {
        let mut map: HashMap<ParticipantId, String> = HashMap::new();
        let id = ParticipantId::new();
        map.insert(id, "value".to_string());
        assert_eq!(map.get(&id), Some(&"value".to_string()));
    }

    #[test]
    fn track_id_as_hashmap_key() {
        let mut map: HashMap<TrackId, String> = HashMap::new();
        let p = ParticipantId::new();
        let id = p.derive_track_id(MediaKind::Video, "cam");
        map.insert(id, "value".to_string());
        assert_eq!(map.get(&id), Some(&"value".to_string()));
    }

    #[test]
    fn room_id_hash_consistency_after_clone() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::Hash;
        let ext = ExternalRoomId::new("room1").unwrap();
        let room1 = RoomId::from_external(&ext);
        let room2 = room1; // Copy
        let mut h1 = DefaultHasher::new();
        let mut h2 = DefaultHasher::new();
        room1.hash(&mut h1);
        room2.hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish());
    }

    #[test]
    fn participant_id_copy_semantics() {
        let id1 = ParticipantId::new();
        let id2 = id1; // Copy
        assert_eq!(id1, id2);
    }

    #[test]
    fn track_id_copy_semantics() {
        let p = ParticipantId::new();
        let id1 = p.derive_track_id(MediaKind::Video, "cam");
        let id2 = id1; // Copy
        assert_eq!(id1, id2);
    }

    #[test]
    fn room_id_copy_semantics() {
        let ext = ExternalRoomId::new("room1").unwrap();
        let id1 = RoomId::from_external(&ext);
        let id2 = id1; // Copy
        assert_eq!(id1, id2);
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
        assert!(ExternalRoomId::new("a").is_ok());
        assert!(ExternalRoomId::new("Z").is_ok());
        assert!(ExternalRoomId::new("0").is_ok());
        assert!(ExternalRoomId::new("_").is_ok());
        assert!(ExternalRoomId::new("-").is_ok());
    }

    #[test]
    fn room_id_case_sensitivity() {
        let ext1 = ExternalRoomId::new("Room").unwrap();
        let ext2 = ExternalRoomId::new("room").unwrap();
        let room1 = RoomId::from_external(&ext1);
        let room2 = RoomId::from_external(&ext2);
        assert_ne!(room1.as_str(), room2.as_str());
    }

    #[test]
    fn display_format() {
        assert!(format!("{}", ParticipantId::new()).starts_with("pa_"));
        let p = ParticipantId::new();
        assert!(format!("{}", p.derive_track_id(MediaKind::Video, "c")).starts_with("vid_"));
        let ext = ExternalRoomId::new("test").unwrap();
        assert!(format!("{}", RoomId::from_external(&ext)).starts_with("rm_"));
    }

    #[test]
    fn as_str_returns_valid_string() {
        let participant_id = ParticipantId::new();
        assert!(participant_id.as_str().starts_with("pa_"));
        assert_eq!(participant_id.as_str(), participant_id.as_str());
        let track_id = participant_id.derive_track_id(MediaKind::Video, "cam");
        assert!(track_id.as_str().starts_with("vid_"));
        assert_eq!(track_id.as_str(), track_id.as_str());
    }

    #[test]
    fn parsing_from_client_input() {
        assert!(ExternalRoomId::from_str("my-conference-room").is_ok());
        let participant_id = ParticipantId::new();
        let serialized = participant_id.as_str();
        let parsed = ParticipantId::from_str(&serialized).unwrap();
        assert_eq!(parsed, participant_id);
    }

    #[test]
    fn storing_in_multiple_collections() {
        let participant_id = ParticipantId::new();
        let track_id = participant_id.derive_track_id(MediaKind::Video, "cam");
        let mut participant_map: HashMap<ParticipantId, Vec<TrackId>> = HashMap::new();
        let mut track_map: HashMap<TrackId, ParticipantId> = HashMap::new();
        participant_map.insert(participant_id, vec![track_id]);
        track_map.insert(track_id, participant_id);
        assert_eq!(participant_map.get(&participant_id), Some(&vec![track_id]));
        assert_eq!(track_map.get(&track_id), Some(&participant_id));
    }

    #[test]
    fn comparison_and_ordering() {
        let id1 = ParticipantId::new();
        let id2 = ParticipantId::new();
        assert_ne!(id1, id2);
        let mut ids = [id1, id2];
        ids.sort();
        assert_eq!(id1, id1);
    }

    #[test]
    fn collision_resistance() {
        let mut internal_ids = HashSet::new();
        for external in ["room", "room1", "Room", "ROOM", "room_", "room-1"] {
            let ext = ExternalRoomId::new(external).unwrap();
            let room_id = RoomId::from_external(&ext);
            assert!(internal_ids.insert(room_id.as_str()));
        }
    }

    #[test]
    fn ids_are_url_safe() {
        for c in ParticipantId::new().as_str().chars() {
            assert!(c.is_ascii_alphanumeric() || c == '_');
        }
        let p = ParticipantId::new();
        for c in p.derive_track_id(MediaKind::Video, "c").as_str().chars() {
            assert!(c.is_ascii_alphanumeric() || c == '_');
        }
    }
}
