//! Authentication keys and the shared public project registry.

use crate::identity::{ApiKeyId, ProjectId};
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fmt, str::FromStr};

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
}
