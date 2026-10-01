use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

const KEY_BYTES: usize = 32;
const KEY_TEXT_LEN: usize = 52;
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const REDACTED: &str = "[REDACTED]";

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum KeyValidationError {
    #[error("invalid key prefix; expected {expected}")]
    InvalidPrefix { expected: &'static str },
    #[error("invalid key length; expected {expected}, got {actual}")]
    InvalidLength { expected: usize, actual: usize },
    #[error("unsupported key encoding version")]
    UnsupportedVersion,
    #[error("invalid Crockford Base32 key encoding")]
    InvalidEncoding,
    #[error("key encoding has nonzero high padding bits")]
    NonzeroPadding,
    #[error("invalid Ed25519 verifying key")]
    InvalidVerifyingKey,
}

fn crockford_value(byte: u8) -> Option<u8> {
    match byte.to_ascii_uppercase() {
        b'O' => Some(0),
        b'I' | b'L' => Some(1),
        byte => CROCKFORD
            .iter()
            .position(|candidate| *candidate == byte)
            .and_then(|index| u8::try_from(index).ok()),
    }
}

fn encode_key(bytes: &[u8; KEY_BYTES]) -> String {
    let mut encoded = String::with_capacity(KEY_TEXT_LEN);
    let mut buffer = 0u16;
    let mut bits = 4u8;
    for byte in bytes {
        buffer = (buffer << 8) | u16::from(*byte);
        bits = bits.saturating_add(8);
        while bits >= 5 {
            bits = bits.saturating_sub(5);
            let value = (buffer >> bits) & 0x1f;
            let character = CROCKFORD.get(usize::from(value)).copied().unwrap_or(b'?');
            encoded.push(char::from(character));
            buffer &= (1u16 << bits).wrapping_sub(1);
        }
    }
    encoded
}

fn decode_key(value: &str) -> Result<[u8; KEY_BYTES], KeyValidationError> {
    if value.len() != KEY_TEXT_LEN {
        return Err(KeyValidationError::InvalidLength {
            expected: KEY_TEXT_LEN,
            actual: value.len(),
        });
    }

    let mut decoded = Vec::with_capacity(KEY_BYTES);
    let mut buffer = 0u16;
    let mut bits = 0u8;
    for (index, byte) in value.bytes().enumerate() {
        let digit = crockford_value(byte).ok_or(KeyValidationError::InvalidEncoding)?;
        if index == 0 {
            if digit > 1 {
                return Err(KeyValidationError::NonzeroPadding);
            }
            buffer = u16::from(digit);
            bits = 1;
        } else {
            buffer = (buffer << 5) | u16::from(digit);
            bits = bits.saturating_add(5);
        }
        while bits >= 8 {
            bits = bits.saturating_sub(8);
            let byte =
                u8::try_from(buffer >> bits).map_err(|_| KeyValidationError::InvalidEncoding)?;
            decoded.push(byte);
            buffer &= (1u16 << bits).wrapping_sub(1);
        }
    }
    decoded
        .try_into()
        .map_err(|_| KeyValidationError::InvalidEncoding)
}

fn parse_payload<'a>(value: &'a str, prefix: &'static str) -> Result<&'a str, KeyValidationError> {
    let Some(versioned) = value
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('_'))
    else {
        return Err(KeyValidationError::InvalidPrefix { expected: prefix });
    };
    versioned
        .strip_prefix('0')
        .or_else(|| versioned.strip_prefix('O'))
        .or_else(|| versioned.strip_prefix('o'))
        .ok_or(KeyValidationError::UnsupportedVersion)
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct ApiVerifyingKey([u8; KEY_BYTES]);

impl ApiVerifyingKey {
    pub fn from_bytes(bytes: [u8; KEY_BYTES]) -> Result<Self, KeyValidationError> {
        VerifyingKey::from_bytes(&bytes).map_err(|_| KeyValidationError::InvalidVerifyingKey)?;
        Ok(Self(bytes))
    }

    #[doc(hidden)]
    pub const fn from_bytes_unchecked(bytes: [u8; KEY_BYTES]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; KEY_BYTES] {
        &self.0
    }

    pub fn as_str(&self) -> String {
        format!("pk_0{}", encode_key(&self.0))
    }
}

impl fmt::Display for ApiVerifyingKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.as_str())
    }
}

impl fmt::Debug for ApiVerifyingKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.as_str())
    }
}

impl FromStr for ApiVerifyingKey {
    type Err = KeyValidationError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_bytes(decode_key(parse_payload(value, "pk")?)?)
    }
}

impl Serialize for ApiVerifyingKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.as_str())
    }
}

impl<'de> Deserialize<'de> for ApiVerifyingKey {
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

    pub fn verifying_key(&self) -> ApiVerifyingKey {
        ApiVerifyingKey(SigningKey::from_bytes(&self.0).verifying_key().to_bytes())
    }

    #[doc(hidden)]
    pub fn sign_message(&self, message: &[u8]) -> [u8; 64] {
        SigningKey::from_bytes(&self.0).sign(message).to_bytes()
    }

    pub fn to_secret_string(&self) -> String {
        format!("sk_0{}", encode_key(&self.0))
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
        Ok(Self(decode_key(parse_payload(value, "sk")?)?))
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
    fn key_vectors_round_trip_and_aliases_canonicalize() {
        let signing = SIGNING_KEY.clone();
        let public = SIGNING_KEY.verifying_key();
        let signing_text = signing.to_secret_string();
        let public_text = public.as_str();
        assert_eq!(
            signing_text,
            "sk_017B1P6EYZZATC2X88JQMJBP2SH24972PJYSJD4CQ0EXC0CEAWZV0"
        );
        assert_eq!(
            public_text,
            "pk_01NTTK00R5C8APZAMQZPKS5J0EEGEW5SF7PN64CJTY0GTD3VGEM8T"
        );
        assert_eq!(signing_text.parse::<ApiSigningKey>().unwrap(), signing);
        assert_eq!(public_text.parse::<ApiVerifyingKey>().unwrap(), public);
        assert_eq!(
            public_text
                .to_ascii_lowercase()
                .parse::<ApiVerifyingKey>()
                .unwrap(),
            public
        );
        assert_eq!(
            signing_text
                .replace('0', "O")
                .parse::<ApiSigningKey>()
                .unwrap(),
            signing
        );
    }

    #[test]
    fn malformed_keys_and_nonzero_high_bits_are_rejected() {
        let public = SIGNING_KEY.verifying_key().as_str();
        assert!(matches!(
            public.replacen("pk", "sk", 1).parse::<ApiVerifyingKey>(),
            Err(KeyValidationError::InvalidPrefix { .. })
        ));
        assert!(matches!(
            public
                .replacen("pk_0", "pk_1", 1)
                .parse::<ApiVerifyingKey>(),
            Err(KeyValidationError::UnsupportedVersion)
        ));
        let short = public.chars().take(55).collect::<String>();
        assert!(matches!(
            short.parse::<ApiVerifyingKey>(),
            Err(KeyValidationError::InvalidLength { .. })
        ));
        let payload = public.chars().skip(5).collect::<String>();
        let nonzero_padding = format!("pk_0Z{payload}");
        assert!(matches!(
            nonzero_padding.parse::<ApiVerifyingKey>(),
            Err(KeyValidationError::NonzeroPadding)
        ));
        assert!(
            public
                .replacen('D', "U", 1)
                .parse::<ApiVerifyingKey>()
                .is_err()
        );
    }
}
