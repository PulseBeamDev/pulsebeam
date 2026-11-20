//! Authentication keys and the shared public key registry.

use crate::identity::{ApiKeyId, ParticipantId, ProjectId, RoomId};
use data_encoding::BASE64URL_NOPAD;
use ed25519_dalek::{Signature, VerifyingKey};
pub use pulsebeam_auth::api_key::{ApiKey, ApiKeyError};
pub use pulsebeam_auth::grants;
pub use pulsebeam_auth::identity::TokenId;
pub use pulsebeam_auth::keys::{ApiPublicKey, ApiSigningKey, KeyValidationError};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::LazyLock};

#[cfg(test)]
const KEY_BYTES: usize = 32;
#[cfg(test)]
const REDACTED: &str = "[REDACTED]";
pub use pulsebeam_auth::MAX_COMPACT_TOKEN_LEN;

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
pub struct VerifiedPrincipal {
    project_id: ProjectId,
    participant_id: ParticipantId,
    token_id: TokenId,
    expiry: AuthorizationExpiry,
    grants: grants::GrantSet,
}

impl VerifiedPrincipal {
    pub fn project_id(&self) -> ProjectId {
        self.project_id
    }

    pub fn participant_id(&self) -> &ParticipantId {
        &self.participant_id
    }

    pub fn token_id(&self) -> TokenId {
        self.token_id
    }

    pub fn expiry(&self) -> AuthorizationExpiry {
        self.expiry
    }

    pub fn authorize_room(
        self,
        room: RoomId,
    ) -> Result<VerifiedAuthorization, RoomAuthorizationError> {
        if !self
            .grants
            .allows(grants::Action::Join, room.as_str(), None, None, None)
        {
            return Err(RoomAuthorizationError::Denied);
        }
        Ok(self.bind_room(room))
    }

    pub fn authorize_existing(
        self,
        accepted: &VerifiedAuthorization,
    ) -> Result<VerifiedAuthorization, RoomAuthorizationError> {
        if self.project_id != accepted.project_id || self.participant_id != accepted.participant_id
        {
            return Err(RoomAuthorizationError::Denied);
        }
        Ok(self.bind_room(accepted.room_id))
    }

    fn bind_room(self, room: RoomId) -> VerifiedAuthorization {
        VerifiedAuthorization {
            project_id: self.project_id,
            room_id: room,
            participant_id: self.participant_id,
            token_id: self.token_id,
            expiry: self.expiry,
            grants: self.grants,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RoomAuthorizationError {
    #[error("authorization does not permit the selected room")]
    Denied,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedAuthorization {
    pub project_id: ProjectId,
    pub room_id: RoomId,
    pub participant_id: ParticipantId,
    pub token_id: TokenId,
    pub expiry: AuthorizationExpiry,
    pub grants: grants::GrantSet,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenHeader {
    alg: String,
    kid: ApiKeyId,
    typ: String,
    #[serde(default)]
    crit: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ParticipantClaims {
    iss: ProjectId,
    aud: String,
    sub: ParticipantId,
    exp: u64,
    jti: Box<str>,
    grants: grants::GrantSet,
}

fn decode_json<T: serde::de::DeserializeOwned>(segment: &str) -> Result<T, TokenError> {
    let bytes = BASE64URL_NOPAD
        .decode(segment.as_bytes())
        .map_err(|_| TokenError::Malformed)?;
    if bytes.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
        return Err(TokenError::Malformed);
    }
    serde_json::from_slice(&bytes).map_err(|error| {
        if error.is_data() {
            TokenError::Invalid
        } else {
            TokenError::Malformed
        }
    })
}

pub fn verify_participant_token(
    registry: &KeyRegistry,
    token: &str,
    room: RoomId,
    now: u64,
) -> Result<VerifiedAuthorization, TokenError> {
    let principal = authenticate_participant_token(registry, token, now)?;
    principal
        .authorize_room(room)
        .map_err(|_| TokenError::Invalid)
}

/// Verify the current credential profile without materializing a room incarnation.
pub fn authenticate_participant_token(
    registry: &KeyRegistry,
    token: &str,
    now: u64,
) -> Result<VerifiedPrincipal, TokenError> {
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

    let header: TokenHeader = decode_json(encoded_header)?;
    if header.alg != "EdDSA" || header.typ != "pb+jwt" || !header.crit.is_empty() {
        return Err(TokenError::Invalid);
    }
    let claims: ParticipantClaims = decode_json(encoded_claims)?;
    if claims.aud != "pb" {
        return Err(TokenError::Invalid);
    }
    let token_id = claims.jti.parse().map_err(|_| TokenError::Invalid)?;
    let expiry = AuthorizationExpiry::from_unix_seconds(claims.exp);
    if expiry.is_expired_at(now) {
        return Err(TokenError::Expired);
    }

    if claims.iss != registry.project_id() {
        return Err(TokenError::Invalid);
    }
    let public_key = registry
        .public_key(&header.kid)
        .ok_or(TokenError::Invalid)?;
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

    Ok(VerifiedPrincipal {
        project_id: claims.iss,
        participant_id: claims.sub,
        token_id,
        expiry,
        grants: claims.grants,
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
mod grants_tests;

#[cfg(test)]
mod tests {
    #![allow(
        clippy::arithmetic_side_effects,
        clippy::expect_used,
        clippy::panic,
        clippy::unwrap_used
    )]

    use super::*;

    fn verify_participant_token(
        registry: &KeyRegistry,
        token: &str,
        now: u64,
    ) -> Result<VerifiedAuthorization, TokenError> {
        super::verify_participant_token(registry, token, RoomId::new("general").unwrap(), now)
    }

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
                "{{\"iss\":\"{iss}\",\"aud\":\"{aud}\",\"sub\":\"{sub}\",\"exp\":{exp},\"jti\":\"00000000-0000-4000-8000-000000000001\",\"grants\":[{{\"rooms\":[\"{room}\"],\"allow\":[\"join\",\"publish\",\"discover\",\"subscribe\"]}}]{claims_extra}}}"
            ),
            &DEVELOPMENT_API_SIGNING_KEY,
        )
    }

    #[test]
    fn jti_must_be_present_once_and_be_a_canonical_rfc_uuid_v4() {
        const VALID: &str = "abcd0123-4567-489a-8bcd-0123456789ab";
        let registry = development_registry();
        let header = format!(
            r#"{{"alg":"EdDSA","kid":"{}","typ":"pb+jwt"}}"#,
            DEVELOPMENT_API_KEY_ID.as_str()
        );
        let verify = |extra: &str| {
            let claims = format!(
                r#"{{"iss":"{}","aud":"pb","sub":"alice","exp":2000,"grants":[{{"rooms":["general"],"allow":["join"]}}]{extra}}}"#,
                DEVELOPMENT_PROJECT_ID.as_str()
            );
            let token = signed_token(&header, &claims, &DEVELOPMENT_API_SIGNING_KEY);
            verify_participant_token(&registry, &token, 1000)
        };
        assert_eq!(
            verify(&format!(r#", "jti":"{VALID}""#))
                .unwrap()
                .token_id
                .to_string(),
            VALID
        );
        for bad in [
            "",
            r#", "jti":null"#,
            r#", "jti":123"#,
            r#", "jti":"ABCD0123-4567-489A-8BCD-0123456789AB""#,
            r#", "jti":"abcd01234567489a8bcd0123456789ab""#,
            r#", "jti":"abcd0123-4567-789a-8bcd-0123456789ab""#,
            r#", "jti":"abcd0123-4567-489a-7bcd-0123456789ab""#,
            r#", "jti":"abcd0123-4567-489a-8bcd-0123456789ab", "jti":"abcd0123-4567-489a-8bcd-0123456789ab""#,
        ] {
            assert_eq!(verify(bad), Err(TokenError::Invalid));
        }
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
    fn development_jwt_requires_grants_and_derives_canonical_identity() {
        let room = RoomId::new("general").unwrap();
        let participant = ParticipantId::new("alice").unwrap();
        let token = "eyJhbGciOiJFZERTQSIsImtpZCI6ImtpZF8wMDAwMDAwMDAwMVIwMTAwMDAwMDAwMDAwMDgiLCJ0eXAiOiJwYitqd3QifQ.eyJpc3MiOiJwXzAwMDAwMDAwMDAxUjAxMDAwMDAwMDAwMDAwNCIsImF1ZCI6InBiIiwic3ViIjoiYWxpY2UiLCJyb29tIjoiZ2VuZXJhbCIsImV4cCI6MjAwMH0.Bp8UJVINeP0cqUiG_0qSk4wjqDg5MGZqRcBel3Qy176HoTy0OQvTpTp5Uuav5k2Wlsgdd58rPt6DLiWVt0U1AQ";

        assert_eq!(
            authenticate_participant_token(&development_registry(), token, 1_999),
            Err(TokenError::Invalid)
        );
        let token = profile_token("", "");
        let token = token.as_str();
        let authorization =
            verify_participant_token(&development_registry(), token, 1_999).unwrap();
        assert_eq!(authorization.project_id, *DEVELOPMENT_PROJECT_ID);
        assert_eq!(authorization.room_id, room);
        assert_eq!(authorization.participant_id, participant);
        assert_eq!(authorization.expiry.unix_seconds(), 2_000);
        assert!(!format!("{authorization:?}").contains(token));

        let principal =
            authenticate_participant_token(&development_registry(), token, 1_999).unwrap();
        assert_eq!(principal.project_id(), authorization.project_id);
        assert_eq!(principal.participant_id(), &participant);
        assert_eq!(principal.expiry(), authorization.expiry);
        assert!(!format!("{principal:?}").contains(token));
        assert_eq!(
            principal.clone().authorize_room(room).unwrap(),
            authorization
        );
        assert_eq!(
            principal.authorize_room(RoomId::new("other").unwrap()),
            Err(RoomAuthorizationError::Denied)
        );
        assert_eq!(
            authenticate_participant_token(&development_registry(), token, 2_000),
            Err(TokenError::Expired)
        );
    }

    #[test]
    fn complete_encoded_token_budget_includes_header_separators_and_signature() {
        let header = format!(
            "{{\"alg\":\"EdDSA\",\"kid\":\"{}\",\"typ\":\"pb+jwt\"}}",
            DEVELOPMENT_API_KEY_ID.as_str()
        );
        let claims = format!(
            "{{\"iss\":\"{}\",\"aud\":\"pb\",\"sub\":\"p\",\"exp\":2000,\"jti\":\"00000000-0000-4000-8000-000000000001\",\"grants\":[{{\"rooms\":[\"general\"],\"allow\":[\"join\"]}}]}}",
            DEVELOPMENT_PROJECT_ID.as_str()
        );
        let fixed = BASE64URL_NOPAD.encode(claims.as_bytes()).len() + 2 + 86;
        let header_bytes = (8192 - fixed) * 3 / 4;
        let padded_header = format!("{}{header}", " ".repeat(header_bytes - header.len()));
        let at_limit = signed_token(&padded_header, &claims, &DEVELOPMENT_API_SIGNING_KEY);
        assert_eq!(at_limit.len(), 8192);
        assert!(verify_participant_token(&development_registry(), &at_limit, 1_000).is_ok());
        let oversized = signed_token(
            &format!(" {padded_header}"),
            &claims,
            &DEVELOPMENT_API_SIGNING_KEY,
        );
        assert!(oversized.len() > 8192);
        assert_eq!(
            verify_participant_token(&development_registry(), &oversized, 1_000),
            Err(TokenError::Malformed)
        );
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
                r"not\nvalid",
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
                r"not\u0085valid",
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
    fn jwt_accepts_aliases_and_empty_critical_header() {
        let project_alias = DEVELOPMENT_PROJECT_ID.as_str().replace('0', "O");
        let key_alias = DEVELOPMENT_API_KEY_ID.as_str().to_ascii_lowercase();
        let token = signed_token(
            &format!(
                "{{\"alg\":\"EdDSA\",\"kid\":\"{key_alias}\",\"typ\":\"pb+jwt\",\"crit\":[]}}"
            ),
            &format!(
                "{{\"iss\":\"{project_alias}\",\"aud\":\"pb\",\"sub\":\"alice\",\"exp\":2000,\"jti\":\"00000000-0000-4000-8000-000000000001\",\"grants\":[{{\"rooms\":[\"general\"],\"allow\":[\"join\"]}}]}}"
            ),
            &DEVELOPMENT_API_SIGNING_KEY,
        );
        let authorization =
            verify_participant_token(&development_registry(), &token, 1_000).unwrap();
        assert_eq!(authorization.project_id, *DEVELOPMENT_PROJECT_ID);
    }

    #[test]
    fn selected_room_and_existing_resource_have_distinct_grant_checks() {
        let header = format!(
            "{{\"alg\":\"EdDSA\",\"kid\":\"{}\",\"typ\":\"pb+jwt\"}}",
            DEVELOPMENT_API_KEY_ID.as_str()
        );
        let token = |grants: &str, expiration| {
            signed_token(
                &header,
                &format!(
                    "{{\"iss\":\"{}\",\"aud\":\"pb\",\"sub\":\"alice\",\"exp\":{expiration},\"jti\":\"00000000-0000-4000-8000-000000000001\",\"grants\":{grants}}}",
                    DEVELOPMENT_PROJECT_ID.as_str()
                ),
                &DEVELOPMENT_API_SIGNING_KEY,
            )
        };
        let registry = development_registry();
        let credential = token(r#"[{"rooms":["Team_Main"],"allow":["join"]}]"#, 2000);
        let principal = authenticate_participant_token(&registry, &credential, 1000).unwrap();
        let room = RoomId::new("Team_Main").unwrap();
        let accepted = principal.clone().authorize_room(room.clone()).unwrap();
        for denied in ["team", "team_main", "elsewhere"] {
            assert_eq!(
                principal
                    .clone()
                    .authorize_room(RoomId::new(denied).unwrap()),
                Err(RoomAuthorizationError::Denied)
            );
        }
        let credential = token("[]", 3000);
        let revoked = authenticate_participant_token(&registry, &credential, 1000).unwrap();
        assert_eq!(
            revoked.clone().authorize_room(room),
            Err(RoomAuthorizationError::Denied)
        );
        let replacement = revoked.clone().authorize_existing(&accepted).unwrap();
        assert_eq!(replacement.room_id, accepted.room_id);
        assert_eq!(replacement.participant_id, accepted.participant_id);
        assert_eq!(replacement.grants, grants::GrantSet::default());
        assert_eq!(replacement.expiry.unix_seconds(), 3000);
        let mut other = accepted.clone();
        other.participant_id = ParticipantId::new("other").unwrap();
        assert_eq!(
            revoked.clone().authorize_existing(&other),
            Err(RoomAuthorizationError::Denied)
        );
    }

    #[test]
    fn jwt_schema_rejects_extensions_duplicate_grants_and_malformed_filters() {
        let registry = development_registry();
        for token in [
            profile_token(",\"future\":{\"enabled\":true}", ""),
            profile_token("", ",\"room\":\"general\""),
            profile_token("", ",\"future\":[[[[1]]]]"),
            profile_token("", ",\"grants\":[]"),
            profile_token("", ",\"grants\":null"),
        ] {
            assert!(authenticate_participant_token(&registry, &token, 1000).is_err());
        }
        let header = format!(
            "{{\"alg\":\"EdDSA\",\"kid\":\"{}\",\"typ\":\"pb+jwt\"}}",
            DEVELOPMENT_API_KEY_ID.as_str()
        );
        let header_array =
            serde_json::json!(["EdDSA", DEVELOPMENT_API_KEY_ID.as_str(), "pb+jwt", []]).to_string();
        let claims = format!(
            "{{\"iss\":\"{}\",\"aud\":\"pb\",\"sub\":\"alice\",\"exp\":2000,\"jti\":\"00000000-0000-4000-8000-000000000001\",\"grants\":[{{\"rooms\":[\"*\"],\"allow\":[\"join\"]}}]}}",
            DEVELOPMENT_PROJECT_ID.as_str()
        );
        for invalid in ["stage.alice", "café", "alice\n", &"a".repeat(37)] {
            let claims = claims.replace("\"alice\"", &serde_json::to_string(invalid).unwrap());
            let token = signed_token(&header, &claims, &DEVELOPMENT_API_SIGNING_KEY);
            assert!(matches!(
                authenticate_participant_token(&registry, &token, 1000),
                Err(TokenError::Invalid)
            ));
        }
        let array_token = signed_token(&header_array, &claims, &DEVELOPMENT_API_SIGNING_KEY);
        assert!(authenticate_participant_token(&registry, &array_token, 1000).is_err());
        let claims_array = serde_json::json!([DEVELOPMENT_PROJECT_ID.as_str(), "pb", "alice", 2000, [{"rooms":["*"],"allow":["join"]}]]).to_string();
        let array_token = signed_token(&header, &claims_array, &DEVELOPMENT_API_SIGNING_KEY);
        assert!(authenticate_participant_token(&registry, &array_token, 1000).is_err());
        for grants in [
            r#"[{"rooms":["*"],"allow":["join"],"label":null}]"#,
            r#"[{"rooms":["*"],"rooms":["general"],"allow":["join"]}]"#,
            r#"[{"rooms":["*"],"allow":["unknown"]}]"#,
            r#"[{"rooms":["*"],"allow":["subscribe"],"kind":{"nested":"audio"}}]"#,
        ] {
            let claims = format!(
                "{{\"iss\":\"{}\",\"aud\":\"pb\",\"sub\":\"alice\",\"exp\":2000,\"jti\":\"00000000-0000-4000-8000-000000000001\",\"grants\":{grants}}}",
                DEVELOPMENT_PROJECT_ID.as_str()
            );
            let token = signed_token(&header, &claims, &DEVELOPMENT_API_SIGNING_KEY);
            assert!(authenticate_participant_token(&registry, &token, 1000).is_err());
        }
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
