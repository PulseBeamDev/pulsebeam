use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;
use thiserror::Error;

pub use str0m::change::{SdpAnswer, SdpOffer};
pub use str0m::error::SdpError;
pub use str0m::{Rtc, RtcError};

#[derive(Debug)]
pub struct IngressUDPPacket {
    pub raw: Bytes,
    pub src: SocketAddr,
}

#[derive(Debug)]
pub struct EgressUDPPacket {
    pub raw: Bytes,
    pub dst: SocketAddr,
}

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

/// Represents a validated Group ID.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")] // Use try_from for deserialization validation
pub struct GroupId(String);

impl GroupId {
    const MAX_LEN: usize = 20;

    /// Creates a new GroupId from a string, validating it.
    pub fn new(id: String) -> Result<Self, IdValidationError> {
        validate_id_string(&id, Self::MAX_LEN)?;
        Ok(GroupId(id))
    }

    /// Returns a reference to the inner string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// Implement FromStr for parsing from strings (e.g., path segments)
impl FromStr for GroupId {
    type Err = IdValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        GroupId::new(s.to_string())
    }
}

// Implement Display for easy printing
impl fmt::Display for GroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// Implement TryFrom<String> for serde deserialization
impl TryFrom<String> for GroupId {
    type Error = IdValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        GroupId::new(value)
    }
}

// Implement AsRef<str> for easy string reference access
impl AsRef<str> for GroupId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

/// Represents a validated Peer ID.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")] // Use try_from for deserialization validation
pub struct PeerId(String);

impl PeerId {
    const MAX_LEN: usize = 20;

    /// Creates a new PeerId from a string, validating it.
    pub fn new(id: String) -> Result<Self, IdValidationError> {
        validate_id_string(&id, Self::MAX_LEN)?;
        Ok(PeerId(id))
    }

    /// Returns a reference to the inner string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// Implement FromStr for parsing from strings (e.g., path segments)
impl FromStr for PeerId {
    type Err = IdValidationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        PeerId::new(s.to_string())
    }
}

// Implement Display for easy printing
impl fmt::Display for PeerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// Implement TryFrom<String> for serde deserialization
impl TryFrom<String> for PeerId {
    type Error = IdValidationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        PeerId::new(value)
    }
}

// Implement AsRef<str> for easy string reference access
impl AsRef<str> for PeerId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_id_valid() {
        assert!(GroupId::new("valid_id-123".to_string()).is_ok());
        assert!(GroupId::new("a".to_string()).is_ok());
        assert!(GroupId::new("A".to_string()).is_ok());
        assert!(GroupId::new("0".to_string()).is_ok());
        assert!(GroupId::new("_".to_string()).is_ok());
        assert!(GroupId::new("-".to_string()).is_ok());
        assert!(GroupId::new("a".repeat(20)).is_ok()); // Max length
    }

    #[test]
    fn group_id_invalid_length() {
        let err = GroupId::new("a".repeat(21)).unwrap_err(); // Too long
        assert!(matches!(err, IdValidationError::TooLong(20)));
    }

    #[test]
    fn group_id_invalid_characters() {
        let err = GroupId::new("invalid!".to_string()).unwrap_err();
        assert!(matches!(err, IdValidationError::InvalidCharacters));
        let err = GroupId::new(" space".to_string()).unwrap_err();
        assert!(matches!(err, IdValidationError::InvalidCharacters));
        let err = GroupId::new("ñ_id".to_string()).unwrap_err();
        assert!(matches!(err, IdValidationError::InvalidCharacters));
    }

    #[test]
    fn group_id_empty() {
        let err = GroupId::new("".to_string()).unwrap_err();
        assert!(matches!(err, IdValidationError::Empty));
    }

    #[test]
    fn peer_id_valid() {
        assert!(PeerId::new("valid_peer-456".to_string()).is_ok());
        assert!(PeerId::new("b".to_string()).is_ok());
        assert!(PeerId::new("B".to_string()).is_ok());
        assert!(PeerId::new("1".to_string()).is_ok());
        assert!(PeerId::new("_".to_string()).is_ok());
        assert!(PeerId::new("-".to_string()).is_ok());
        assert!(PeerId::new("b".repeat(20)).is_ok()); // Max length
    }

    #[test]
    fn peer_id_invalid_length() {
        let err = PeerId::new("b".repeat(21)).unwrap_err(); // Too long
        assert!(matches!(err, IdValidationError::TooLong(20)));
    }

    #[test]
    fn peer_id_invalid_characters() {
        let err = PeerId::new("invalid@".to_string()).unwrap_err();
        assert!(matches!(err, IdValidationError::InvalidCharacters));
        let err = PeerId::new(" space".to_string()).unwrap_err();
        assert!(matches!(err, IdValidationError::InvalidCharacters));
        let err = PeerId::new("ñ_peer".to_string()).unwrap_err();
        assert!(matches!(err, IdValidationError::InvalidCharacters));
    }

    #[test]
    fn peer_id_empty() {
        let err = PeerId::new("".to_string()).unwrap_err();
        assert!(matches!(err, IdValidationError::Empty));
    }

    #[test]
    fn group_id_serde_deserialize() {
        let json = r#""valid_id-123""#;
        let group_id: Result<GroupId, serde_json::Error> = serde_json::from_str(json);
        assert!(group_id.is_ok());
        assert_eq!(group_id.unwrap().as_str(), "valid_id-123");

        let invalid_json = r#""invalid!""#;
        let group_id: Result<GroupId, serde_json::Error> = serde_json::from_str(invalid_json);
        assert!(group_id.is_err());
        let err_str = group_id.unwrap_err().to_string();
        assert!(err_str.contains("ID contains invalid characters"));

        let too_long_json = format!(r#""{}""#, "a".repeat(21));
        let group_id: Result<GroupId, serde_json::Error> = serde_json::from_str(&too_long_json);
        assert!(group_id.is_err());
        let err_str = group_id.unwrap_err().to_string();
        assert!(err_str.contains("ID exceeds maximum length of 20 characters"));
    }

    #[test]
    fn peer_id_serde_deserialize() {
        let json = r#""valid_peer-456""#;
        let peer_id: Result<PeerId, serde_json::Error> = serde_json::from_str(json);
        assert!(peer_id.is_ok());
        assert_eq!(peer_id.unwrap().as_str(), "valid_peer-456");

        let invalid_json = r#""invalid@""#;
        let peer_id: Result<PeerId, serde_json::Error> = serde_json::from_str(invalid_json);
        assert!(peer_id.is_err());
        let err_str = peer_id.unwrap_err().to_string();
        assert!(err_str.contains("ID contains invalid characters"));

        let too_long_json = format!(r#""{}""#, "b".repeat(21));
        let peer_id: Result<PeerId, serde_json::Error> = serde_json::from_str(&too_long_json);
        assert!(peer_id.is_err());
        let err_str = peer_id.unwrap_err().to_string();
        assert!(err_str.contains("ID exceeds maximum length of 20 characters"));
    }
}
