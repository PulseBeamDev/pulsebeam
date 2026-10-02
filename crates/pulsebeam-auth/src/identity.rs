use data_encoding::Encoding;
use data_encoding_macro::new_encoding;
use uuid::{Uuid, Variant, Version};

pub const UUID_TEXT_LEN: usize = 26;
const EXTERNAL_ID_MAX_LEN: usize = 36;
const VERSION: u8 = b'0';
pub(crate) const CROCKFORD: Encoding = new_encoding! {
    symbols: "0123456789ABCDEFGHJKMNPQRSTVWXYZ",
    translate_from: "abcdefghjkmnpqrstvwxyzIiLlOo",
    translate_to: "ABCDEFGHJKMNPQRSTVWXYZ111100",
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum IdValidationError {
    #[error("ID is empty")]
    Empty,
    #[error("ID exceeds maximum length of {0}")]
    TooLong(usize),
    #[error("ID contains invalid characters")]
    InvalidCharacters,
    #[error("invalid ID prefix; expected {expected}")]
    InvalidPrefix { expected: &'static str },
    #[error("invalid ID length; expected {expected}, got {actual}")]
    InvalidLength { expected: usize, actual: usize },
    #[error("unsupported ID encoding version")]
    UnsupportedVersion,
    #[error("invalid Crockford Base32 encoding")]
    InvalidEncoding,
    #[error("invalid UUID variant")]
    InvalidUuidVariant,
    #[error("invalid UUID version")]
    InvalidUuidVersion,
}

pub fn encode_uuid(uuid: Uuid) -> String {
    CROCKFORD.encode(uuid.as_bytes())
}

pub fn decode_uuid(value: &str) -> Result<Uuid, IdValidationError> {
    if value.len() != UUID_TEXT_LEN {
        return Err(IdValidationError::InvalidLength {
            expected: UUID_TEXT_LEN,
            actual: value.len(),
        });
    }
    let decoded = CROCKFORD
        .decode(value.as_bytes())
        .ok()
        .and_then(|bytes| <[u8; 16]>::try_from(bytes).ok())
        .ok_or(IdValidationError::InvalidEncoding)?;
    Ok(Uuid::from_bytes(decoded))
}

pub fn format_id(prefix: &str, uuid: Uuid) -> String {
    let mut value = String::with_capacity(prefix.len().saturating_add(UUID_TEXT_LEN + 2));
    value.push_str(prefix);
    value.push('_');
    value.push(char::from(VERSION));
    value.push_str(&encode_uuid(uuid));
    value
}

pub fn parse_id(
    value: &str,
    prefix: &'static str,
    expected_version: Version,
) -> Result<Uuid, IdValidationError> {
    let Some(encoded) = value
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('_'))
    else {
        return Err(IdValidationError::InvalidPrefix { expected: prefix });
    };
    let Some(payload) = encoded
        .strip_prefix(char::from(VERSION))
        .or_else(|| encoded.strip_prefix('O'))
        .or_else(|| encoded.strip_prefix('o'))
    else {
        return Err(IdValidationError::UnsupportedVersion);
    };
    let uuid = decode_uuid(payload)?;
    if uuid.get_variant() != Variant::RFC4122 {
        return Err(IdValidationError::InvalidUuidVariant);
    }
    if uuid.get_version() != Some(expected_version) {
        return Err(IdValidationError::InvalidUuidVersion);
    }
    Ok(uuid)
}

pub fn validate_external(value: &str) -> Result<(), IdValidationError> {
    if value.is_empty() {
        return Err(IdValidationError::Empty);
    }
    if value.len() > EXTERNAL_ID_MAX_LEN {
        return Err(IdValidationError::TooLong(EXTERNAL_ID_MAX_LEN));
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(IdValidationError::InvalidCharacters);
    }
    Ok(())
}
