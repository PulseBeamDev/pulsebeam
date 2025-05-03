use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::fmt::{self, Debug};
use std::hash::Hash;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use str0m::media::{MediaKind, Mid, Simulcast};
use thiserror::Error;

pub use str0m::change::{SdpAnswer, SdpOffer};
pub use str0m::error::SdpError;
pub use str0m::{Rtc, RtcError};

#[derive(Debug)]
pub struct UDPPacket {
    pub raw: Bytes,
    pub src: SocketAddr,
    pub dst: SocketAddr,
}

#[derive(Debug)]
pub struct EgressUDPPacket {
    pub raw: Bytes,
    pub dst: SocketAddr,
}

#[derive(Debug)]
pub struct TrackIn {
    pub mid: Mid,
    pub kind: MediaKind,
    pub simulcast: Option<Simulcast>,
}

#[derive(Hash, PartialEq, Eq, Debug, Clone)]
pub struct TrackKey {
    pub origin: Arc<ParticipantId>,
    pub mid: Mid,
}
impl ActorId for TrackKey {}

#[derive(thiserror::Error, Debug)]
pub enum ActorError {
    #[error("unknown error: {0}")]
    Unknown(String),
}

pub type ActorResult = Result<(), ActorError>;

pub trait ActorId: Hash + Eq + PartialEq + Debug {}

#[derive(Debug, Error)]
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

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct RoomId(String);

impl RoomId {
    const MAX_LEN: usize = 20;

    pub fn new(id: String) -> Result<Self, IdValidationError> {
        validate_id_string(&id, Self::MAX_LEN)?;
        Ok(RoomId(id))
    }

    /// Returns a reference to the inner string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for RoomId {
    type Err = IdValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        RoomId::new(s.to_string())
    }
}

impl fmt::Display for RoomId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl TryFrom<String> for RoomId {
    type Error = IdValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        RoomId::new(value)
    }
}

impl AsRef<str> for RoomId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(try_from = "String")]
pub struct ParticipantId(String);

impl ParticipantId {
    const MAX_LEN: usize = 20;

    pub fn new(id: String) -> Result<Self, IdValidationError> {
        validate_id_string(&id, Self::MAX_LEN)?;
        Ok(ParticipantId(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ParticipantId {
    type Err = IdValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ParticipantId::new(s.to_string())
    }
}

impl fmt::Display for ParticipantId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl TryFrom<String> for ParticipantId {
    type Error = IdValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        ParticipantId::new(value)
    }
}

impl AsRef<str> for ParticipantId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_id_valid() {
        assert!(RoomId::new("valid_id-123".to_string()).is_ok());
        assert!(RoomId::new("a".to_string()).is_ok());
        assert!(RoomId::new("A".to_string()).is_ok());
        assert!(RoomId::new("0".to_string()).is_ok());
        assert!(RoomId::new("_".to_string()).is_ok());
        assert!(RoomId::new("-".to_string()).is_ok());
        assert!(RoomId::new("a".repeat(20)).is_ok()); // Max length
    }

    #[test]
    fn room_id_invalid_length() {
        let err = RoomId::new("a".repeat(21)).unwrap_err(); // Too long
        assert!(matches!(err, IdValidationError::TooLong(20)));
    }

    #[test]
    fn room_id_invalid_characters() {
        let err = RoomId::new("invalid!".to_string()).unwrap_err();
        assert!(matches!(err, IdValidationError::InvalidCharacters));
        let err = RoomId::new(" space".to_string()).unwrap_err();
        assert!(matches!(err, IdValidationError::InvalidCharacters));
        let err = RoomId::new("ñ_id".to_string()).unwrap_err();
        assert!(matches!(err, IdValidationError::InvalidCharacters));
    }

    #[test]
    fn room_id_empty() {
        let err = RoomId::new("".to_string()).unwrap_err();
        assert!(matches!(err, IdValidationError::Empty));
    }

    #[test]
    fn participant_id_valid() {
        assert!(ParticipantId::new("valid_peer-456".to_string()).is_ok());
        assert!(ParticipantId::new("b".to_string()).is_ok());
        assert!(ParticipantId::new("B".to_string()).is_ok());
        assert!(ParticipantId::new("1".to_string()).is_ok());
        assert!(ParticipantId::new("_".to_string()).is_ok());
        assert!(ParticipantId::new("-".to_string()).is_ok());
        assert!(ParticipantId::new("b".repeat(20)).is_ok()); // Max length
    }

    #[test]
    fn participant_id_invalid_length() {
        let err = ParticipantId::new("b".repeat(21)).unwrap_err(); // Too long
        assert!(matches!(err, IdValidationError::TooLong(20)));
    }

    #[test]
    fn participant_id_invalid_characters() {
        let err = ParticipantId::new("invalid@".to_string()).unwrap_err();
        assert!(matches!(err, IdValidationError::InvalidCharacters));
        let err = ParticipantId::new(" space".to_string()).unwrap_err();
        assert!(matches!(err, IdValidationError::InvalidCharacters));
        let err = ParticipantId::new("ñ_peer".to_string()).unwrap_err();
        assert!(matches!(err, IdValidationError::InvalidCharacters));
    }

    #[test]
    fn participant_id_empty() {
        let err = ParticipantId::new("".to_string()).unwrap_err();
        assert!(matches!(err, IdValidationError::Empty));
    }

    #[test]
    fn room_id_serde_deserialize() {
        let json = r#""valid_id-123""#;
        let room_id: Result<RoomId, serde_json::Error> = serde_json::from_str(json);
        assert!(room_id.is_ok());
        assert_eq!(room_id.unwrap().as_str(), "valid_id-123");

        let invalid_json = r#""invalid!""#;
        let room_id: Result<RoomId, serde_json::Error> = serde_json::from_str(invalid_json);
        assert!(room_id.is_err());
        let err_str = room_id.unwrap_err().to_string();
        assert!(err_str.contains("ID contains invalid characters"));

        let too_long_json = format!(r#""{}""#, "a".repeat(21));
        let room_id: Result<RoomId, serde_json::Error> = serde_json::from_str(&too_long_json);
        assert!(room_id.is_err());
        let err_str = room_id.unwrap_err().to_string();
        assert!(err_str.contains("ID exceeds maximum length of 20 characters"));
    }

    #[test]
    fn participant_id_serde_deserialize() {
        let json = r#""valid_peer-456""#;
        let participant_id: Result<ParticipantId, serde_json::Error> = serde_json::from_str(json);
        assert!(participant_id.is_ok());
        assert_eq!(participant_id.unwrap().as_str(), "valid_peer-456");

        let invalid_json = r#""invalid@""#;
        let participant_id: Result<ParticipantId, serde_json::Error> =
            serde_json::from_str(invalid_json);
        assert!(participant_id.is_err());
        let err_str = participant_id.unwrap_err().to_string();
        assert!(err_str.contains("ID contains invalid characters"));

        let too_long_json = format!(r#""{}""#, "b".repeat(21));
        let participant_id: Result<ParticipantId, serde_json::Error> =
            serde_json::from_str(&too_long_json);
        assert!(participant_id.is_err());
        let err_str = participant_id.unwrap_err().to_string();
        assert!(err_str.contains("ID exceeds maximum length of 20 characters"));
    }
}
