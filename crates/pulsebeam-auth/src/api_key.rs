//! Offline V0 API key: project UUIDv7 || key UUIDv7 || Ed25519 seed.
//! Canonical wire form: `key_0` followed by 103 Crockford Base32 digits.
use crate::identity::CROCKFORD;
use crate::keys::ApiSigningKey;
use std::{fmt, str::FromStr};
use uuid::{Uuid, Variant, Version};

const PREFIX: &str = "key_0";
const ENCODED_BYTES: usize = 64;
const ENCODED_DIGITS: usize = 103;
const REDACTED: &str = "[REDACTED]";

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ApiKeyError {
    #[error("invalid API key prefix or version")]
    InvalidPrefix,
    #[error("invalid API key length")]
    InvalidLength,
    #[error("invalid API key encoding or padding")]
    InvalidEncoding,
    #[error("invalid API key UUIDv7 identity")]
    InvalidIdentity,
}

/// Server-only signing credential. Never log, serialize, or send to clients.
///
/// V0 is the concatenation of 16-byte project UUIDv7, 16-byte key UUIDv7,
/// and 32-byte Ed25519 private seed, encoded together, MSB-first. The final
/// Base32 digit has three zero low bits.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey {
    project_id: [u8; 16],
    key_id: [u8; 16],
    seed: [u8; 32],
}

impl ApiKey {
    pub fn new(
        project_id: [u8; 16],
        key_id: [u8; 16],
        seed: [u8; 32],
    ) -> Result<Self, ApiKeyError> {
        for id in [project_id, key_id] {
            let uuid = Uuid::from_bytes(id);
            if uuid.get_variant() != Variant::RFC4122
                || uuid.get_version() != Some(Version::SortRand)
            {
                return Err(ApiKeyError::InvalidIdentity);
            }
        }
        Ok(Self {
            project_id,
            key_id,
            seed,
        })
    }

    pub fn from_signing_key(
        project_id: [u8; 16],
        key_id: [u8; 16],
        signing_key: &ApiSigningKey,
    ) -> Result<Self, ApiKeyError> {
        Self::new(project_id, key_id, *signing_key.seed_bytes())
    }

    pub fn project_id_bytes(&self) -> &[u8; 16] {
        &self.project_id
    }

    pub fn key_id_bytes(&self) -> &[u8; 16] {
        &self.key_id
    }

    pub fn signing_key(&self) -> ApiSigningKey {
        ApiSigningKey::from_seed(self.seed)
    }

    /// Explicit secret export; Debug/Display remain redacted.
    pub fn to_secret_string(&self) -> String {
        let mut payload = [0u8; ENCODED_BYTES];
        payload[..16].copy_from_slice(&self.project_id);
        payload[16..32].copy_from_slice(&self.key_id);
        payload[32..].copy_from_slice(&self.seed);
        format!("{PREFIX}{}", CROCKFORD.encode(&payload))
    }
}

impl FromStr for ApiKey {
    type Err = ApiKeyError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let text = value.strip_prefix(PREFIX).ok_or(ApiKeyError::InvalidPrefix)?;
        if text.len() != ENCODED_DIGITS {
            return Err(ApiKeyError::InvalidLength);
        }
        let bytes = CROCKFORD
            .decode(text.as_bytes())
            .map_err(|_| ApiKeyError::InvalidEncoding)?;
        let data: [u8; ENCODED_BYTES] = bytes
            .try_into()
            .map_err(|_| ApiKeyError::InvalidEncoding)?;
        // Reject noncanonical unused padding bits, including codec aliases
        // that would alter the underlying secret's textual representation.
        let canonical = CROCKFORD.encode(&data);
        let normalized = text
            .chars()
            .map(|c| match c.to_ascii_uppercase() {
                'O' => '0',
                'I' | 'L' => '1',
                other => other,
            })
            .collect::<String>();
        if canonical != normalized {
            return Err(ApiKeyError::InvalidEncoding);
        }
        let project_id = data[..16].try_into().map_err(|_| ApiKeyError::InvalidEncoding)?;
        let key_id = data[16..32].try_into().map_err(|_| ApiKeyError::InvalidEncoding)?;
        let seed = data[32..].try_into().map_err(|_| ApiKeyError::InvalidEncoding)?;
        Self::new(project_id, key_id, seed)
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

impl fmt::Display for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::arithmetic_side_effects)]
    use super::*;

    #[test]
    fn development_api_key_round_trip() {
        let key: ApiKey = concat!(
            "key_00000000001R0100000000000040000000007004000000000001",
            "9TRDHKQQZTPK0QA24NX4JXGPC8H29RNMQPCK935R3QB033JQ7YR0"
        )
        .parse()
        .unwrap();
        assert_eq!(
            key.project_id_bytes(),
            &[0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 1]
        );
        assert_eq!(
            key.key_id_bytes(),
            &[0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 2]
        );
        assert!(key.to_secret_string().starts_with("key_0"));
        assert_eq!(key.to_secret_string().len(), 108);
        assert_eq!(format!("{key:?}"), REDACTED);
    }

    #[test]
    fn rejects_padding_version_and_wrong_uuid_kind() {
        let good = concat!(
            "key_00000000001R0100000000000040000000007004000000000001",
            "9TRDHKQQZTPK0QA24NX4JXGPC8H29RNMQPCK935R3QB033JQ7YR0"
        );
        assert!(good.replacen("key_0", "key_1", 1).parse::<ApiKey>().is_err());
        assert!(good[..good.len() - 1].parse::<ApiKey>().is_err());
        assert!(good.replace("key_0", "sk_0").parse::<ApiKey>().is_err());
        assert!(format!("{}1", &good[..good.len() - 1]).parse::<ApiKey>().is_err());
    }
}
