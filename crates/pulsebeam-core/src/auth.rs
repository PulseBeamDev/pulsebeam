//! Authentication keys and the shared public project registry.

use crate::identity::{
    ApiKeyId, ParticipantExternalId, ParticipantId, ProjectId, RoomExternalId, RoomId,
};
use data_encoding::BASE64URL_NOPAD;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize, de};
use std::{collections::HashMap, fmt, str::FromStr};

const KEY_BYTES: usize = 32;
const KEY_TEXT_LEN: usize = 52;
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const REDACTED: &str = "[REDACTED]";
pub const MAX_COMPACT_TOKEN_LEN: usize = 2 * 1024;

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

pub const DEVELOPMENT_PROJECT_ID: ProjectId =
    ProjectId::from_bytes([0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 1]);
pub const DEVELOPMENT_API_KEY_ID: ApiKeyId =
    ApiKeyId::from_bytes([0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, 2]);
pub const DEVELOPMENT_API_SIGNING_KEY: ApiSigningKey = ApiSigningKey::from_seed([
    0x9d, 0x61, 0xb1, 0x9d, 0xef, 0xfd, 0x5a, 0x60, 0xba, 0x84, 0x4a, 0xf4, 0x92, 0xec, 0x2c, 0xc4,
    0x44, 0x49, 0xc5, 0x69, 0x7b, 0x32, 0x69, 0x19, 0x70, 0x3b, 0xac, 0x03, 0x1c, 0xae, 0x7f, 0x60,
]);
pub const DEVELOPMENT_API_VERIFYING_KEY: ApiVerifyingKey = ApiVerifyingKey::from_bytes_unchecked([
    0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64, 0x07, 0x3a,
    0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68, 0xf7, 0x07, 0x51, 0x1a,
]);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedAuthorization {
    pub project_id: ProjectId,
    pub room_external_id: RoomExternalId,
    pub room_id: RoomId,
    pub participant_external_id: ParticipantExternalId,
    pub participant_id: ParticipantId,
    pub exp: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TokenError {
    #[error("malformed authorization token")]
    Malformed,
    #[error("invalid authorization token")]
    Invalid,
    #[error("authorization token expired")]
    Expired,
}

enum UniqueJson {
    Null,
    Bool,
    Number(serde_json::Number),
    String(String),
    Array(Vec<Self>),
    Object(HashMap<String, Self>),
}

impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor;

        impl<'de> de::Visitor<'de> for Visitor {
            type Value = UniqueJson;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON value without duplicate object members")
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(UniqueJson::Null)
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(UniqueJson::Null)
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                let _ = value;
                Ok(UniqueJson::Bool)
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(UniqueJson::Number(value.into()))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(UniqueJson::Number(value.into()))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                serde_json::Number::from_f64(value)
                    .map(UniqueJson::Number)
                    .ok_or_else(|| E::custom("invalid JSON number"))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(UniqueJson::String(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(UniqueJson::String(value))
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: de::SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element()? {
                    values.push(value);
                }
                Ok(UniqueJson::Array(values))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: de::MapAccess<'de>,
            {
                let mut values = HashMap::new();
                while let Some((key, value)) = map.next_entry()? {
                    if values.insert(key, value).is_some() {
                        return Err(de::Error::custom("duplicate JSON member"));
                    }
                }
                Ok(UniqueJson::Object(values))
            }
        }

        deserializer.deserialize_any(Visitor)
    }
}

fn parse_object(segment: &str) -> Result<HashMap<String, UniqueJson>, TokenError> {
    let bytes = BASE64URL_NOPAD
        .decode(segment.as_bytes())
        .map_err(|_| TokenError::Malformed)?;
    match serde_json::from_slice(&bytes).map_err(|_| TokenError::Malformed)? {
        UniqueJson::Object(object) => Ok(object),
        _ => Err(TokenError::Malformed),
    }
}

fn take_string(object: &mut HashMap<String, UniqueJson>, name: &str) -> Result<String, TokenError> {
    match object.remove(name) {
        Some(UniqueJson::String(value)) => Ok(value),
        _ => Err(TokenError::Invalid),
    }
}

pub fn verify_participant_token(
    registry: &ProjectRegistry,
    token: &str,
    now: u64,
) -> Result<VerifiedAuthorization, TokenError> {
    if token.len() > MAX_COMPACT_TOKEN_LEN {
        return Err(TokenError::Malformed);
    }
    let mut segments = token.split('.');
    let (Some(encoded_header), Some(encoded_claims), Some(encoded_signature), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return Err(TokenError::Malformed);
    };
    if encoded_header.is_empty() || encoded_claims.is_empty() || encoded_signature.is_empty() {
        return Err(TokenError::Malformed);
    }

    let mut header = parse_object(encoded_header)?;
    if take_string(&mut header, "alg")? != "EdDSA" || take_string(&mut header, "typ")? != "pb+jwt" {
        return Err(TokenError::Invalid);
    }
    let key_id = take_string(&mut header, "kid")?
        .parse::<ApiKeyId>()
        .map_err(|_| TokenError::Invalid)?;
    if let Some(crit) = header.remove("crit") {
        match crit {
            UniqueJson::Array(values) if values.is_empty() => {}
            _ => return Err(TokenError::Invalid),
        }
    }

    let mut claims = parse_object(encoded_claims)?;
    if claims.contains_key("iat") || take_string(&mut claims, "aud")? != "pb" {
        return Err(TokenError::Invalid);
    }
    let project_id = take_string(&mut claims, "iss")?
        .parse::<ProjectId>()
        .map_err(|_| TokenError::Invalid)?;
    let participant_external_id = ParticipantExternalId::new(&take_string(&mut claims, "sub")?)
        .map_err(|_| TokenError::Invalid)?;
    let room_external_id =
        RoomExternalId::new(&take_string(&mut claims, "room")?).map_err(|_| TokenError::Invalid)?;
    let exp = match claims.remove("exp") {
        Some(UniqueJson::Number(value)) => value.as_u64().ok_or(TokenError::Invalid)?,
        _ => return Err(TokenError::Invalid),
    };
    if exp <= now {
        return Err(TokenError::Expired);
    }

    let verifying_key = registry
        .verifying_key(&project_id, &key_id)
        .ok_or(TokenError::Invalid)?;
    let signature = BASE64URL_NOPAD
        .decode(encoded_signature.as_bytes())
        .ok()
        .and_then(|bytes| Signature::from_slice(&bytes).ok())
        .ok_or(TokenError::Malformed)?;
    let signing_input = format!("{encoded_header}.{encoded_claims}");
    VerifyingKey::from_bytes(verifying_key.as_bytes())
        .map_err(|_| TokenError::Invalid)?
        .verify_strict(signing_input.as_bytes(), &signature)
        .map_err(|_| TokenError::Invalid)?;

    let room_id = RoomId::derive(&project_id, &room_external_id);
    let participant_id = ParticipantId::derive(&room_id, &participant_external_id);
    Ok(VerifiedAuthorization {
        project_id,
        room_external_id,
        room_id,
        participant_external_id,
        participant_id,
        exp,
    })
}

#[derive(Serialize)]
struct DevelopmentHeader<'a> {
    alg: &'a str,
    kid: ApiKeyId,
    typ: &'a str,
}

#[derive(Serialize)]
struct DevelopmentClaims<'a> {
    iss: ProjectId,
    aud: &'a str,
    sub: &'a str,
    room: &'a str,
    exp: u64,
}

pub fn mint_development_token(
    room: &RoomExternalId,
    participant: &ParticipantExternalId,
    exp: u64,
) -> Result<String, TokenError> {
    let header = serde_json::to_vec(&DevelopmentHeader {
        alg: "EdDSA",
        kid: DEVELOPMENT_API_KEY_ID,
        typ: "pb+jwt",
    })
    .map_err(|_| TokenError::Invalid)?;
    let claims = serde_json::to_vec(&DevelopmentClaims {
        iss: DEVELOPMENT_PROJECT_ID,
        aud: "pb",
        sub: participant.as_str(),
        room: room.as_str(),
        exp,
    })
    .map_err(|_| TokenError::Invalid)?;
    let encoded_header = BASE64URL_NOPAD.encode(&header);
    let encoded_claims = BASE64URL_NOPAD.encode(&claims);
    let signing_input = format!("{encoded_header}.{encoded_claims}");
    let signature =
        SigningKey::from_bytes(&DEVELOPMENT_API_SIGNING_KEY.0).sign(signing_input.as_bytes());
    Ok(format!(
        "{signing_input}.{}",
        BASE64URL_NOPAD.encode(&signature.to_bytes())
    ))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectKey {
    pub key_id: ApiKeyId,
    pub verifying_key: ApiVerifyingKey,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectKeys {
    pub project_id: ProjectId,
    pub keys: Vec<ProjectKey>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryDocument {
    projects: Vec<ProjectKeys>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RegistryError {
    #[error("invalid project registry JSON: {0}")]
    InvalidJson(String),
    #[error("duplicate project ID {0}")]
    DuplicateProject(ProjectId),
    #[error("duplicate API key ID {0}")]
    DuplicateKey(ApiKeyId),
    #[error("project {0} has no verifying keys")]
    MissingKeys(ProjectId),
    #[error("expected exactly one project, found {0}")]
    ExpectedOneProject(usize),
}

#[derive(Clone, Debug)]
pub struct ProjectRegistry {
    projects: Vec<ProjectKeys>,
    project_index: HashMap<ProjectId, usize>,
    key_index: HashMap<ApiKeyId, (usize, usize)>,
}

impl ProjectRegistry {
    pub fn new(projects: Vec<ProjectKeys>) -> Result<Self, RegistryError> {
        let mut project_index = HashMap::with_capacity(projects.len());
        let mut key_index = HashMap::new();
        for (project_position, project) in projects.iter().enumerate() {
            if project.keys.is_empty() {
                return Err(RegistryError::MissingKeys(project.project_id));
            }
            if project_index
                .insert(project.project_id, project_position)
                .is_some()
            {
                return Err(RegistryError::DuplicateProject(project.project_id));
            }
            for (key_position, key) in project.keys.iter().enumerate() {
                if key_index
                    .insert(key.key_id, (project_position, key_position))
                    .is_some()
                {
                    return Err(RegistryError::DuplicateKey(key.key_id));
                }
            }
        }
        Ok(Self {
            projects,
            project_index,
            key_index,
        })
    }

    pub fn parse_json(json: &str) -> Result<Self, RegistryError> {
        let document: RegistryDocument = serde_json::from_str(json)
            .map_err(|error| RegistryError::InvalidJson(error.to_string()))?;
        Self::new(document.projects)
    }

    pub fn to_pretty_json(&self) -> Result<String, RegistryError> {
        serde_json::to_string_pretty(&RegistryDocument {
            projects: self.projects.clone(),
        })
        .map_err(|error| RegistryError::InvalidJson(error.to_string()))
    }

    pub fn projects(&self) -> &[ProjectKeys] {
        &self.projects
    }

    pub fn project(&self, project_id: &ProjectId) -> Option<&ProjectKeys> {
        self.project_index
            .get(project_id)
            .and_then(|position| self.projects.get(*position))
    }

    pub fn verifying_key(
        &self,
        project_id: &ProjectId,
        key_id: &ApiKeyId,
    ) -> Option<&ApiVerifyingKey> {
        let (project_position, key_position) = self.key_index.get(key_id)?;
        let project = self.projects.get(*project_position)?;
        if project.project_id != *project_id {
            return None;
        }
        project
            .keys
            .get(*key_position)
            .map(|key| &key.verifying_key)
    }

    pub fn only_project(&self) -> Result<&ProjectKeys, RegistryError> {
        if self.projects.len() != 1 {
            return Err(RegistryError::ExpectedOneProject(self.projects.len()));
        }
        self.projects
            .first()
            .ok_or(RegistryError::ExpectedOneProject(0))
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateSigningBundle {
    pub project_id: ProjectId,
    pub key_id: ApiKeyId,
    pub signing_key: ApiSigningKey,
}

impl fmt::Debug for PrivateSigningBundle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PrivateSigningBundle")
            .field("project_id", &self.project_id)
            .field("key_id", &self.key_id)
            .field("signing_key", &REDACTED)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::arithmetic_side_effects,
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_used
    )]

    use super::*;

    fn development_registry() -> ProjectRegistry {
        ProjectRegistry::new(vec![ProjectKeys {
            project_id: DEVELOPMENT_PROJECT_ID,
            keys: vec![ProjectKey {
                key_id: DEVELOPMENT_API_KEY_ID,
                verifying_key: DEVELOPMENT_API_VERIFYING_KEY,
            }],
        }])
        .unwrap()
    }

    fn signed_token(header: &str, claims: &str, signing_key: &ApiSigningKey) -> String {
        let encoded_header = BASE64URL_NOPAD.encode(header.as_bytes());
        let encoded_claims = BASE64URL_NOPAD.encode(claims.as_bytes());
        let signing_input = format!("{encoded_header}.{encoded_claims}");
        let signature = SigningKey::from_bytes(&signing_key.0).sign(signing_input.as_bytes());
        format!(
            "{signing_input}.{}",
            BASE64URL_NOPAD.encode(&signature.to_bytes())
        )
    }

    fn profile_token(header_extra: &str, claims_extra: &str) -> String {
        profile_token_with(
            "EdDSA",
            &DEVELOPMENT_API_KEY_ID.as_str(),
            "pb+jwt",
            &DEVELOPMENT_PROJECT_ID.as_str(),
            "pb",
            "alice",
            "general",
            "2000",
            header_extra,
            claims_extra,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn profile_token_with(
        alg: &str,
        kid: &str,
        typ: &str,
        iss: &str,
        aud: &str,
        sub: &str,
        room: &str,
        exp: &str,
        header_extra: &str,
        claims_extra: &str,
    ) -> String {
        signed_token(
            &format!("{{\"alg\":\"{alg}\",\"kid\":\"{kid}\",\"typ\":\"{typ}\"{header_extra}}}"),
            &format!(
                "{{\"iss\":\"{iss}\",\"aud\":\"{aud}\",\"sub\":\"{sub}\",\"room\":\"{room}\",\"exp\":{exp}{claims_extra}}}"
            ),
            &DEVELOPMENT_API_SIGNING_KEY,
        )
    }

    fn signing(seed: u8) -> ApiSigningKey {
        ApiSigningKey::from_seed([seed; KEY_BYTES])
    }

    fn project(last: u8) -> ProjectId {
        ProjectId::from_bytes([0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, last])
    }

    fn key_id(last: u8) -> ApiKeyId {
        ApiKeyId::from_bytes([0, 0, 0, 0, 0, 0, 0x70, 0, 0x80, 0, 0, 0, 0, 0, 0, last])
    }

    fn entry(project_id: ProjectId, key_ids: &[u8]) -> ProjectKeys {
        ProjectKeys {
            project_id,
            keys: key_ids
                .iter()
                .map(|id| ProjectKey {
                    key_id: key_id(*id),
                    verifying_key: signing(*id).verifying_key(),
                })
                .collect(),
        }
    }

    #[test]
    fn key_vectors_round_trip_and_aliases_canonicalize() {
        let signing = DEVELOPMENT_API_SIGNING_KEY.clone();
        let public = DEVELOPMENT_API_VERIFYING_KEY;
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
        let public = DEVELOPMENT_API_VERIFYING_KEY.as_str();
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

    #[test]
    fn signing_material_is_redacted_from_formatting_and_errors() {
        let signing = DEVELOPMENT_API_SIGNING_KEY.clone();
        let secret = signing.to_secret_string();
        assert_eq!(format!("{signing}"), REDACTED);
        assert_eq!(format!("{signing:?}"), REDACTED);
        assert!(
            !format!(
                "{:?}",
                PrivateSigningBundle {
                    project_id: DEVELOPMENT_PROJECT_ID,
                    key_id: DEVELOPMENT_API_KEY_ID,
                    signing_key: signing,
                }
            )
            .contains(&secret)
        );
        assert!(!format!("{:?}", secret.parse::<ApiVerifyingKey>().unwrap_err()).contains(&secret));
    }

    #[test]
    fn development_constants_form_one_ed25519_pair() {
        assert_eq!(
            DEVELOPMENT_API_SIGNING_KEY.verifying_key(),
            DEVELOPMENT_API_VERIFYING_KEY
        );
        assert_eq!(
            DEVELOPMENT_PROJECT_ID.as_str(),
            "p_00000000001R010000000000004"
        );
        assert_eq!(
            DEVELOPMENT_API_KEY_ID.as_str(),
            "kid_00000000001R010000000000008"
        );
    }

    #[test]
    fn registry_accepts_multiple_projects_and_rotated_keys() {
        let registry =
            ProjectRegistry::new(vec![entry(project(1), &[1, 2]), entry(project(2), &[3])])
                .unwrap();
        assert_eq!(registry.projects().len(), 2);
        assert_eq!(
            registry.verifying_key(&project(1), &key_id(2)),
            Some(&signing(2).verifying_key())
        );
        assert_eq!(registry.verifying_key(&project(2), &key_id(2)), None);
        assert_eq!(registry.verifying_key(&project(1), &key_id(9)), None);
    }

    #[test]
    fn registry_rejects_duplicate_canonical_aliases_and_missing_keys() {
        let registry = ProjectRegistry::new(vec![entry(project(1), &[1]), entry(project(1), &[2])]);
        assert!(matches!(registry, Err(RegistryError::DuplicateProject(_))));
        let registry = ProjectRegistry::new(vec![entry(project(1), &[1]), entry(project(2), &[1])]);
        assert!(matches!(registry, Err(RegistryError::DuplicateKey(_))));
        assert!(matches!(
            ProjectRegistry::new(vec![entry(project(1), &[])]),
            Err(RegistryError::MissingKeys(_))
        ));

        let project_json = serde_json::to_string(&entry(project(1), &[1])).unwrap();
        let aliased = project_json.replace('0', "O");
        let json = format!("{{\"projects\":[{project_json},{aliased}]}}");
        assert!(matches!(
            ProjectRegistry::parse_json(&json),
            Err(RegistryError::DuplicateProject(_))
        ));

        let key_json = serde_json::to_string(&entry(project(1), &[1]).keys[0]).unwrap();
        let aliased_key = key_json.replace('0', "O");
        let json = format!(
            "{{\"projects\":[{{\"project_id\":\"{}\",\"keys\":[{key_json},{aliased_key}]}}]}}",
            project(1)
        );
        assert!(matches!(
            ProjectRegistry::parse_json(&json),
            Err(RegistryError::DuplicateKey(_))
        ));
    }

    #[test]
    fn registry_json_round_trips_and_oss_requires_exactly_one_project() {
        let registry = ProjectRegistry::new(vec![entry(project(1), &[1, 2])]).unwrap();
        let reparsed = ProjectRegistry::parse_json(&registry.to_pretty_json().unwrap()).unwrap();
        assert_eq!(reparsed.only_project().unwrap().project_id, project(1));
        assert!(matches!(
            ProjectRegistry::new(Vec::new()).unwrap().only_project(),
            Err(RegistryError::ExpectedOneProject(0))
        ));
        assert!(matches!(
            ProjectRegistry::new(vec![entry(project(1), &[1]), entry(project(2), &[2])])
                .unwrap()
                .only_project(),
            Err(RegistryError::ExpectedOneProject(2))
        ));
    }

    #[test]
    fn development_jwt_golden_vector_verifies_and_derives_canonical_identity() {
        let room = RoomExternalId::new("general").unwrap();
        let participant = ParticipantExternalId::new("alice").unwrap();
        let token = mint_development_token(&room, &participant, 2_000).unwrap();
        assert_eq!(
            token,
            "eyJhbGciOiJFZERTQSIsImtpZCI6ImtpZF8wMDAwMDAwMDAwMVIwMTAwMDAwMDAwMDAwMDgiLCJ0eXAiOiJwYitqd3QifQ.eyJpc3MiOiJwXzAwMDAwMDAwMDAxUjAxMDAwMDAwMDAwMDAwNCIsImF1ZCI6InBiIiwic3ViIjoiYWxpY2UiLCJyb29tIjoiZ2VuZXJhbCIsImV4cCI6MjAwMH0.Bp8UJVINeP0cqUiG_0qSk4wjqDg5MGZqRcBel3Qy176HoTy0OQvTpTp5Uuav5k2Wlsgdd58rPt6DLiWVt0U1AQ"
        );

        let authorization =
            verify_participant_token(&development_registry(), &token, 1_999).unwrap();
        assert_eq!(authorization.project_id, DEVELOPMENT_PROJECT_ID);
        assert_eq!(authorization.room_external_id, room);
        assert_eq!(authorization.participant_external_id, participant);
        assert_eq!(
            authorization.room_id,
            RoomId::derive(&DEVELOPMENT_PROJECT_ID, &authorization.room_external_id)
        );
        assert_eq!(
            authorization.participant_id,
            ParticipantId::derive(
                &authorization.room_id,
                &authorization.participant_external_id
            )
        );
        assert_eq!(authorization.exp, 2_000);
        assert!(!format!("{authorization:?}").contains(&token));
    }

    #[test]
    fn compact_shape_and_json_are_strict() {
        let registry = development_registry();
        let valid = profile_token("", "");
        for token in [
            "",
            "a.b",
            "a.b.c.d",
            ".b.c",
            &format!("{valid}="),
            &"x".repeat(MAX_COMPACT_TOKEN_LEN.saturating_add(1)),
            &signed_token(
                "{\"alg\":\"EdDSA\",\"alg\":\"EdDSA\",\"kid\":\"x\",\"typ\":\"pb+jwt\"}",
                "{}",
                &DEVELOPMENT_API_SIGNING_KEY,
            ),
            &signed_token(
                &format!(
                    "{{\"alg\":\"EdDSA\",\"kid\":\"{DEVELOPMENT_API_KEY_ID}\",\"typ\":\"pb+jwt\",\"unknown\":{{\"x\":1,\"x\":2}}}}"
                ),
                "{}",
                &DEVELOPMENT_API_SIGNING_KEY,
            ),
            &signed_token(
                &format!(
                    "{{\"alg\":\"EdDSA\",\"kid\":\"{DEVELOPMENT_API_KEY_ID}\",\"typ\":\"pb+jwt\"}}"
                ),
                &format!(
                    "{{\"iss\":\"{DEVELOPMENT_PROJECT_ID}\",\"aud\":\"pb\",\"sub\":\"alice\",\"room\":\"general\",\"exp\":2000,\"exp\":2001}}"
                ),
                &DEVELOPMENT_API_SIGNING_KEY,
            ),
        ] {
            assert!(verify_participant_token(&registry, token, 1_000).is_err());
        }
    }

    #[test]
    fn jwt_profile_rejects_invalid_headers_claims_and_signatures() {
        let registry = development_registry();
        let cases = [
            profile_token(",\"crit\":[\"unknown\"]", ""),
            profile_token("", ",\"iat\":1000"),
            profile_token_with(
                "EdDSA",
                &DEVELOPMENT_API_KEY_ID.as_str(),
                "pb+jwt",
                &DEVELOPMENT_PROJECT_ID.as_str(),
                "other",
                "alice",
                "general",
                "2000",
                "",
                "",
            ),
            profile_token_with(
                "EdDSA",
                &DEVELOPMENT_API_KEY_ID.as_str(),
                "pb+jwt",
                &DEVELOPMENT_PROJECT_ID.as_str(),
                "pb",
                "not valid",
                "general",
                "2000",
                "",
                "",
            ),
            profile_token_with(
                "EdDSA",
                &DEVELOPMENT_API_KEY_ID.as_str(),
                "pb+jwt",
                &DEVELOPMENT_PROJECT_ID.as_str(),
                "pb",
                "alice",
                "not/valid",
                "2000",
                "",
                "",
            ),
            profile_token_with(
                "EdDSA",
                &DEVELOPMENT_API_KEY_ID.as_str(),
                "pb+jwt",
                &DEVELOPMENT_PROJECT_ID.as_str(),
                "pb",
                "alice",
                "general",
                "2000.5",
                "",
                "",
            ),
            profile_token_with(
                "HS256",
                &DEVELOPMENT_API_KEY_ID.as_str(),
                "pb+jwt",
                &DEVELOPMENT_PROJECT_ID.as_str(),
                "pb",
                "alice",
                "general",
                "2000",
                "",
                "",
            ),
            profile_token_with(
                "EdDSA",
                &DEVELOPMENT_API_KEY_ID.as_str(),
                "JWT",
                &DEVELOPMENT_PROJECT_ID.as_str(),
                "pb",
                "alice",
                "general",
                "2000",
                "",
                "",
            ),
            profile_token_with(
                "EdDSA",
                &key_id(9).as_str(),
                "pb+jwt",
                &DEVELOPMENT_PROJECT_ID.as_str(),
                "pb",
                "alice",
                "general",
                "2000",
                "",
                "",
            ),
            profile_token_with(
                "EdDSA",
                &DEVELOPMENT_API_KEY_ID.as_str(),
                "pb+jwt",
                &project(9).as_str(),
                "pb",
                "alice",
                "general",
                "2000",
                "",
                "",
            ),
        ];
        for token in cases {
            assert!(verify_participant_token(&registry, &token, 1_000).is_err());
        }

        let valid = profile_token("", "");
        let mut signature = BASE64URL_NOPAD
            .decode(valid.rsplit('.').next().unwrap().as_bytes())
            .unwrap();
        signature[0] ^= 1;
        let tampered = format!(
            "{}.{}",
            valid.rsplit_once('.').unwrap().0,
            BASE64URL_NOPAD.encode(&signature)
        );
        assert_eq!(
            verify_participant_token(&registry, &tampered, 1_000),
            Err(TokenError::Invalid)
        );
    }

    #[test]
    fn jwt_accepts_aliases_and_unknown_noncritical_members() {
        let project_alias = DEVELOPMENT_PROJECT_ID.as_str().replace('0', "O");
        let key_alias = DEVELOPMENT_API_KEY_ID.as_str().to_ascii_lowercase();
        let token = signed_token(
            &format!(
                "{{\"alg\":\"EdDSA\",\"kid\":\"{key_alias}\",\"typ\":\"pb+jwt\",\"crit\":[],\"future\":{{\"enabled\":true}}}}"
            ),
            &format!(
                "{{\"iss\":\"{project_alias}\",\"aud\":\"pb\",\"sub\":\"alice\",\"room\":\"general\",\"exp\":2000,\"future\":[1,2,3]}}"
            ),
            &DEVELOPMENT_API_SIGNING_KEY,
        );
        let authorization =
            verify_participant_token(&development_registry(), &token, 1_000).unwrap();
        assert_eq!(authorization.project_id, DEVELOPMENT_PROJECT_ID);
    }

    #[test]
    fn jwt_expiry_is_strictly_later_than_admission() {
        let token = profile_token("", "");
        let registry = development_registry();
        assert!(verify_participant_token(&registry, &token, 1_999).is_ok());
        assert_eq!(
            verify_participant_token(&registry, &token, 2_000),
            Err(TokenError::Expired)
        );
        assert_eq!(
            verify_participant_token(&registry, &token, 2_001),
            Err(TokenError::Expired)
        );
    }
}
