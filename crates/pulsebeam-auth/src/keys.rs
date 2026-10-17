use data_encoding::BASE64URL_NOPAD;
use ed25519_dalek::{
    Signer, SigningKey, VerifyingKey,
    pkcs8::{DecodePrivateKey, DecodePublicKey, EncodePrivateKey, EncodePublicKey, KeypairBytes},
};
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

const KEY_BYTES: usize = 32;
const REDACTED: &str = "[REDACTED]";

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum KeyValidationError {
    #[error("invalid Ed25519 PKCS#8 private key")]
    InvalidPrivateKey,
    #[error("invalid Ed25519 SPKI public key")]
    InvalidPublicKey,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ApiPublicKey([u8; KEY_BYTES]);

impl ApiPublicKey {
    pub fn from_bytes(bytes: [u8; KEY_BYTES]) -> Result<Self, KeyValidationError> {
        VerifyingKey::from_bytes(&bytes).map_err(|_| KeyValidationError::InvalidPublicKey)?;
        Ok(Self(bytes))
    }

    #[doc(hidden)]
    pub const fn from_bytes_unchecked(bytes: [u8; KEY_BYTES]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; KEY_BYTES] {
        &self.0
    }

    /// Standard RFC 8410 SubjectPublicKeyInfo DER, base64url without padding.
    #[allow(clippy::expect_used)] // Ed25519 SPKI encoding of a validated key is infallible.
    pub fn as_str(&self) -> String {
        let key = VerifyingKey::from_bytes(&self.0).expect("validated Ed25519 public key");
        let der = key.to_public_key_der().expect("Ed25519 SPKI encoding");
        BASE64URL_NOPAD.encode(der.as_bytes())
    }
}

impl fmt::Display for ApiPublicKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.as_str())
    }
}

impl fmt::Debug for ApiPublicKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.as_str())
    }
}

impl FromStr for ApiPublicKey {
    type Err = KeyValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let der = BASE64URL_NOPAD
            .decode(value.as_bytes())
            .map_err(|_| KeyValidationError::InvalidPublicKey)?;
        let key = VerifyingKey::from_public_key_der(&der)
            .map_err(|_| KeyValidationError::InvalidPublicKey)?;
        Self::from_bytes(key.to_bytes())
    }
}

impl Serialize for ApiPublicKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.as_str())
    }
}

impl<'de> Deserialize<'de> for ApiPublicKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct ApiSigningKey([u8; KEY_BYTES]);

impl ApiSigningKey {
    pub const fn from_seed(seed: [u8; KEY_BYTES]) -> Self {
        Self(seed)
    }

    pub(crate) fn seed_bytes(&self) -> &[u8; KEY_BYTES] {
        &self.0
    }

    pub fn public_key(&self) -> ApiPublicKey {
        ApiPublicKey(SigningKey::from_bytes(&self.0).verifying_key().to_bytes())
    }

    #[doc(hidden)]
    pub fn sign_message(&self, message: &[u8]) -> [u8; 64] {
        SigningKey::from_bytes(&self.0).sign(message).to_bytes()
    }

    /// Standard RFC 8410 PKCS#8 DER, base64url without padding.
    #[allow(clippy::expect_used)] // Ed25519 PKCS#8 encoding of a valid seed is infallible.
    pub fn to_secret_string(&self) -> String {
        // Exclude the optional public key to emit PKCS#8 v1, which Go,
        // Node and Python all import. Let the library own DER encoding.
        let der = KeypairBytes {
            secret_key: self.0,
            public_key: None,
        }
        .to_pkcs8_der()
        .expect("Ed25519 PKCS#8 encoding");
        BASE64URL_NOPAD.encode(der.as_bytes())
    }
}

impl fmt::Display for ApiSigningKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTED)
    }
}

impl fmt::Debug for ApiSigningKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTED)
    }
}

impl FromStr for ApiSigningKey {
    type Err = KeyValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let der = BASE64URL_NOPAD
            .decode(value.as_bytes())
            .map_err(|_| KeyValidationError::InvalidPrivateKey)?;
        let key =
            SigningKey::from_pkcs8_der(&der).map_err(|_| KeyValidationError::InvalidPrivateKey)?;
        Ok(Self(key.to_bytes()))
    }
}

impl Serialize for ApiSigningKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_secret_string())
    }
}

impl<'de> Deserialize<'de> for ApiSigningKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    const SIGNING_KEY: ApiSigningKey = ApiSigningKey::from_seed([
        0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c,
        0xc4, 0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae,
        0x7f, 0x60,
    ]);

    #[test]
    fn standard_der_round_trip() {
        let signing = SIGNING_KEY.clone();
        let public = signing.public_key();
        assert_eq!(
            signing.to_secret_string(),
            "MC4CAQAwBQYDK2VwBCIEIJ1hsZ3v_VpguoRK9JLsLMREScVpezJpGXA7rAMcrn9g"
        );
        assert_eq!(
            public.as_str(),
            "MCowBQYDK2VwAyEA11qYAYKxCrfVS_7TyWQHOg7hcvPapiMlrwIaaPcHURo"
        );
        assert_eq!(
            signing.to_secret_string().parse::<ApiSigningKey>().unwrap(),
            signing
        );
        assert_eq!(public.as_str().parse::<ApiPublicKey>().unwrap(), public);
        assert_eq!(format!("{signing:?}"), REDACTED);
    }

    #[test]
    fn malformed_or_wrong_algorithm_keys_are_rejected() {
        let signing = SIGNING_KEY.clone();
        let public = signing.public_key();
        assert!(public.as_str().parse::<ApiSigningKey>().is_err());
        assert!(signing.to_secret_string().parse::<ApiPublicKey>().is_err());
        assert!(
            signing
                .to_secret_string()
                .get(..20)
                .unwrap_or_default()
                .parse::<ApiSigningKey>()
                .is_err()
        );
        assert!(
            public
                .as_str()
                .get(..20)
                .unwrap_or_default()
                .parse::<ApiPublicKey>()
                .is_err()
        );
    }
}
