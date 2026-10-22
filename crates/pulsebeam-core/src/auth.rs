//! Authentication keys and the shared public key registry.

use crate::identity::{
    ApiKeyId, ParticipantExternalId, ParticipantId, ProjectId, RoomExternalId, RoomId,
};
use data_encoding::BASE64URL_NOPAD;
use ed25519_dalek::{Signature, VerifyingKey};
pub use pulsebeam_auth::api_key::{ApiKey, ApiKeyError};
pub use pulsebeam_auth::keys::{ApiPublicKey, ApiSigningKey, KeyValidationError};
use serde::{Deserialize, Serialize, de};
use std::{collections::HashMap, fmt, sync::LazyLock};

#[cfg(test)]
const KEY_BYTES: usize = 32;
#[cfg(test)]
const REDACTED: &str = "[REDACTED]";
pub const MAX_COMPACT_TOKEN_LEN: usize = 16_384;

// The --dev server and all server SDKs use the same public test key.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DevCredentials {
    project_id: ProjectId,
    key_id: ApiKeyId,
    secret: ApiSigningKey,
    public_key: ApiPublicKey,
}

// This checked-in fixture is validated by the SDK credential tests. A malformed
// embedded fixture is a build defect, not a recoverable user-input error.
#[allow(clippy::expect_used)]
fn load_dev_credentials() -> DevCredentials {
    serde_json::from_str(include_str!("../../../server/auth/development.json"))
        .expect("invalid PulseBeam development credentials")
}

static DEV_CREDENTIALS: LazyLock<DevCredentials> = LazyLock::new(load_dev_credentials);

pub static DEVELOPMENT_PROJECT_ID: LazyLock<ProjectId> =
    LazyLock::new(|| DEV_CREDENTIALS.project_id);
pub static DEVELOPMENT_API_KEY_ID: LazyLock<ApiKeyId> = LazyLock::new(|| DEV_CREDENTIALS.key_id);
pub static DEVELOPMENT_API_SIGNING_KEY: LazyLock<ApiSigningKey> =
    LazyLock::new(|| DEV_CREDENTIALS.secret.clone());
pub static DEVELOPMENT_API_PUBLIC_KEY: LazyLock<ApiPublicKey> =
    LazyLock::new(|| DEV_CREDENTIALS.public_key);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedAuthorization {
    pub project_id: ProjectId,
    pub room_external_id: RoomExternalId,
    pub room_id: RoomId,
    pub participant_external_id: ParticipantExternalId,
    pub participant_id: ParticipantId,
    pub expiry: AuthorizationExpiry,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuthorizationExpiry(u64);

impl AuthorizationExpiry {
    pub fn from_unix_seconds(unix_seconds: u64) -> Self {
        Self(unix_seconds)
    }

    pub fn unix_seconds(self) -> u64 {
        self.0
    }

    pub fn is_expired_at(self, now: u64) -> bool {
        self.0 <= now
    }
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
    registry: &KeyRegistry,
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
    let expiry = AuthorizationExpiry::from_unix_seconds(exp);
    if expiry.is_expired_at(now) {
        return Err(TokenError::Expired);
    }

    if project_id != registry.project_id() {
        return Err(TokenError::Invalid);
    }
    let public_key = registry.public_key(&key_id).ok_or(TokenError::Invalid)?;
    let signature = BASE64URL_NOPAD
        .decode(encoded_signature.as_bytes())
        .ok()
        .and_then(|bytes| Signature::from_slice(&bytes).ok())
        .ok_or(TokenError::Malformed)?;
    let signing_input = format!("{encoded_header}.{encoded_claims}");
    VerifyingKey::from_bytes(public_key.as_bytes())
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
        expiry,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KeyRegistryEntry {
    pub key_id: ApiKeyId,
    pub public_key: ApiPublicKey,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct KeyRegistryDocument {
    project_id: ProjectId,
    keys: Vec<KeyRegistryEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum KeyRegistryError {
    #[error("invalid key registry JSON: {0}")]
    InvalidJson(String),
    #[error("duplicate API key ID {0}")]
    DuplicateKey(ApiKeyId),
    #[error("key registry has no public keys")]
    MissingKeys,
}

#[derive(Clone, Debug)]
pub struct KeyRegistry {
    project_id: ProjectId,
    keys: Vec<KeyRegistryEntry>,
    key_index: HashMap<ApiKeyId, usize>,
}

impl KeyRegistry {
    pub fn new(
        project_id: ProjectId,
        keys: Vec<KeyRegistryEntry>,
    ) -> Result<Self, KeyRegistryError> {
        if keys.is_empty() {
            return Err(KeyRegistryError::MissingKeys);
        }
        let mut key_index = HashMap::with_capacity(keys.len());
        for (position, key) in keys.iter().enumerate() {
            if key_index.insert(key.key_id, position).is_some() {
                return Err(KeyRegistryError::DuplicateKey(key.key_id));
            }
        }
        Ok(Self {
            project_id,
            keys,
            key_index,
        })
    }

    pub fn parse_json(json: &str) -> Result<Self, KeyRegistryError> {
        let document: KeyRegistryDocument = serde_json::from_str(json)
            .map_err(|error| KeyRegistryError::InvalidJson(error.to_string()))?;
        Self::new(document.project_id, document.keys)
    }

    pub fn to_pretty_json(&self) -> Result<String, KeyRegistryError> {
        serde_json::to_string_pretty(&KeyRegistryDocument {
            project_id: self.project_id,
            keys: self.keys.clone(),
        })
        .map_err(|error| KeyRegistryError::InvalidJson(error.to_string()))
    }

    pub fn project_id(&self) -> ProjectId {
        self.project_id
    }

    pub fn keys(&self) -> &[KeyRegistryEntry] {
        &self.keys
    }

    pub fn public_key(&self, key_id: &ApiKeyId) -> Option<&ApiPublicKey> {
        self.key_index
            .get(key_id)
            .and_then(|position| self.keys.get(*position))
            .map(|key| &key.public_key)
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

    fn development_registry() -> KeyRegistry {
        KeyRegistry::new(
            *DEVELOPMENT_PROJECT_ID,
            vec![KeyRegistryEntry {
                key_id: *DEVELOPMENT_API_KEY_ID,
                public_key: *DEVELOPMENT_API_PUBLIC_KEY,
            }],
        )
        .unwrap()
    }

    fn signed_token(header: &str, claims: &str, signing_key: &ApiSigningKey) -> String {
        let encoded_header = BASE64URL_NOPAD.encode(header.as_bytes());
        let encoded_claims = BASE64URL_NOPAD.encode(claims.as_bytes());
        let signing_input = format!("{encoded_header}.{encoded_claims}");
        let signature = signing_key.sign_message(signing_input.as_bytes());
        format!("{signing_input}.{}", BASE64URL_NOPAD.encode(&signature))
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

    fn registry_keys(key_ids: &[u8]) -> Vec<KeyRegistryEntry> {
        key_ids
            .iter()
            .map(|id| KeyRegistryEntry {
                key_id: key_id(*id),
                public_key: signing(*id).public_key(),
            })
            .collect()
    }

    #[test]
    fn signing_material_is_redacted_from_formatting_and_errors() {
        let signing = DEVELOPMENT_API_SIGNING_KEY.clone();
        let secret = signing.to_secret_string();
        assert_eq!(format!("{signing}"), REDACTED);
        assert_eq!(format!("{signing:?}"), REDACTED);
        assert!(!format!("{:?}", secret.parse::<ApiPublicKey>().unwrap_err()).contains(&secret));
    }

    #[test]
    fn development_constants_form_one_ed25519_pair() {
        assert_eq!(
            DEVELOPMENT_API_SIGNING_KEY.public_key(),
            *DEVELOPMENT_API_PUBLIC_KEY
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
    fn registry_accepts_rotated_keys_for_one_project() {
        let registry = KeyRegistry::new(project(1), registry_keys(&[1, 2])).unwrap();
        assert_eq!(registry.project_id(), project(1));
        assert_eq!(registry.keys().len(), 2);
        assert_eq!(
            registry.public_key(&key_id(2)),
            Some(&signing(2).public_key())
        );
        assert_eq!(registry.public_key(&key_id(9)), None);
    }

    #[test]
    fn registry_rejects_duplicate_canonical_aliases_and_missing_keys() {
        assert!(matches!(
            KeyRegistry::new(project(1), registry_keys(&[])),
            Err(KeyRegistryError::MissingKeys)
        ));
        assert!(matches!(
            KeyRegistry::new(project(1), registry_keys(&[1, 1])),
            Err(KeyRegistryError::DuplicateKey(_))
        ));

        let key_json = serde_json::to_string(&registry_keys(&[1])[0]).unwrap();
        let aliased_key = key_json.replace('0', "O");
        let json = format!(
            "{{\"projectId\":\"{}\",\"keys\":[{key_json},{aliased_key}]}}",
            project(1)
        );
        assert!(matches!(
            KeyRegistry::parse_json(&json),
            Err(KeyRegistryError::DuplicateKey(_))
        ));
    }

    #[test]
    fn registry_json_round_trips_single_project_camel_case_schema() {
        let registry = KeyRegistry::new(project(1), registry_keys(&[1, 2])).unwrap();
        let json = registry.to_pretty_json().unwrap();
        assert!(json.contains("\"projectId\""));
        assert!(json.contains("\"keyId\""));
        assert!(json.contains("\"publicKey\""));
        assert!(!json.contains("\"projects\""));
        assert!(!json.contains("verifyingKey"));
        let reparsed = KeyRegistry::parse_json(&json).unwrap();
        assert_eq!(reparsed.project_id(), project(1));
        assert_eq!(reparsed.keys(), registry.keys());
    }

    #[test]
    fn development_jwt_golden_vector_verifies_and_derives_canonical_identity() {
        let room = RoomExternalId::new("general").unwrap();
        let participant = ParticipantExternalId::new("alice").unwrap();
        let token = "eyJhbGciOiJFZERTQSIsImtpZCI6ImtpZF8wMDAwMDAwMDAwMVIwMTAwMDAwMDAwMDAwMDgiLCJ0eXAiOiJwYitqd3QifQ.eyJpc3MiOiJwXzAwMDAwMDAwMDAxUjAxMDAwMDAwMDAwMDAwNCIsImF1ZCI6InBiIiwic3ViIjoiYWxpY2UiLCJyb29tIjoiZ2VuZXJhbCIsImV4cCI6MjAwMH0.Bp8UJVINeP0cqUiG_0qSk4wjqDg5MGZqRcBel3Qy176HoTy0OQvTpTp5Uuav5k2Wlsgdd58rPt6DLiWVt0U1AQ";

        let authorization =
            verify_participant_token(&development_registry(), token, 1_999).unwrap();
        assert_eq!(authorization.project_id, *DEVELOPMENT_PROJECT_ID);
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
        assert_eq!(authorization.expiry.unix_seconds(), 2_000);
        assert!(!format!("{authorization:?}").contains(token));
    }

    #[test]
    fn compact_shape_and_json_are_strict() {
        let registry = development_registry();
        let valid = profile_token("", "");
        let key_id = DEVELOPMENT_API_KEY_ID.as_str();
        let project_id = DEVELOPMENT_PROJECT_ID.as_str();
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
                    "{{\"alg\":\"EdDSA\",\"kid\":\"{key_id}\",\"typ\":\"pb+jwt\",\"unknown\":{{\"x\":1,\"x\":2}}}}"
                ),
                "{}",
                &DEVELOPMENT_API_SIGNING_KEY,
            ),
            &signed_token(
                &format!("{{\"alg\":\"EdDSA\",\"kid\":\"{key_id}\",\"typ\":\"pb+jwt\"}}"),
                &format!(
                    "{{\"iss\":\"{project_id}\",\"aud\":\"pb\",\"sub\":\"alice\",\"room\":\"general\",\"exp\":2000,\"exp\":2001}}"
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
        assert_eq!(authorization.project_id, *DEVELOPMENT_PROJECT_ID);
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
