//! One server-only API key: canonical ProjectId.KeyId.SigningSecret.
//! The component codecs are owned by this crate.
use crate::identity::{format_id, parse_id};
use crate::keys::ApiSigningKey;
use std::{fmt, str::FromStr};
use uuid::{Uuid, Variant, Version};

const REDACTED: &str = "[REDACTED]";

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ApiKeyError {
    #[error("invalid API key structure")]
    InvalidFormat,
    #[error("invalid API key identity")]
    InvalidIdentity,
    #[error("invalid API key signing secret")]
    InvalidSecret,
}

/// Server-only signing credential. Never log or send to clients.
///
/// The API key combines canonical PulseBeam project and key IDs with one
/// standard base64url-encoded PKCS#8 Ed25519 private key.
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

    pub fn project_id(&self) -> String {
        format_id("p", Uuid::from_bytes(self.project_id))
    }

    pub fn key_id(&self) -> String {
        format_id("kid", Uuid::from_bytes(self.key_id))
    }

    pub fn signing_key(&self) -> ApiSigningKey {
        ApiSigningKey::from_seed(self.seed)
    }

    /// Explicit secret export; Debug/Display remain redacted.
    pub fn to_secret_string(&self) -> String {
        format!(
            "{}.{}.{}",
            self.project_id(),
            self.key_id(),
            self.signing_key().to_secret_string()
        )
    }
}

impl FromStr for ApiKey {
    type Err = ApiKeyError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut parts = value.split('.');
        let (Some(project), Some(key), Some(secret), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(ApiKeyError::InvalidFormat);
        };
        let project_id =
            parse_id(project, "p", Version::SortRand).map_err(|_| ApiKeyError::InvalidIdentity)?;
        let key_id =
            parse_id(key, "kid", Version::SortRand).map_err(|_| ApiKeyError::InvalidIdentity)?;
        let secret: ApiSigningKey = secret.parse().map_err(|_| ApiKeyError::InvalidSecret)?;
        Self::from_signing_key(*project_id.as_bytes(), *key_id.as_bytes(), &secret)
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

    fn development_key() -> ApiKey {
        ApiKey::new(
            [0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 1],
            [0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 2],
            [7; 32],
        )
        .unwrap()
    }

    #[test]
    fn compound_api_key_round_trip_and_redaction() {
        let key = development_key();
        let encoded = key.to_secret_string();
        assert_eq!(encoded.len(), 29 + 1 + 31 + 1 + 64);
        assert!(encoded.starts_with("p_0"));
        assert!(encoded.contains(".kid_0"));
        assert!(encoded.contains(".MC4CAQ"));
        assert_eq!(encoded.parse::<ApiKey>().unwrap(), key);
        assert_eq!(format!("{key:?}"), REDACTED);
        assert_eq!(format!("{key}"), REDACTED);
    }

    #[test]
    fn rejects_invalid_components_and_extra_fields() {
        let good = development_key().to_secret_string();
        for candidate in [
            format!("{good}.extra"),
            good.replace("kid_0", "kid_1"),
            good.replace("p_0", "p_1"),
            good.replace("MC4CAQ", "invalid"),
            good.replace("kid_0", "p_0"),
            good.replacen(".", "", 1),
            good.replace("MC4CAQ", "MCowBQ"),
            good.get(..good.len() - 1).unwrap_or_default().to_owned(),
            String::from("key_0bad"),
        ] {
            assert!(candidate.parse::<ApiKey>().is_err());
        }
    }
}
