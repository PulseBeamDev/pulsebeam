use pulsebeam_core::auth::grants::GrantSet;
use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Input {
    pub project_id: String,
    pub key_id: String,
    pub secret: String,
    pub requested_room: String,
    pub grants: GrantSet,
    pub participant: String,
    pub expiration: String,
}

#[derive(Serialize, Deserialize)]
pub struct Golden {
    pub name: String,
    pub input: Input,
    pub canonical_project_id: String,
    pub canonical_key_id: String,
    pub seed_hex: String,
    pub public_key_hex: String,
    pub header: String,
    pub claims: String,
    pub signing_input: String,
    pub signature: String,
    pub token: String,
}

#[derive(PartialEq, Serialize, Deserialize)]
pub struct Rejection {
    pub name: String,
    pub field: String,
    pub value: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub representation: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct Vectors {
    pub version: u32,
    pub base: Input,
    pub valid: Vec<Golden>,
    pub invalid: Vec<Rejection>,
}

#[derive(Deserialize)]
pub struct Case {
    pub name: String,
    pub input: Input,
    pub jti: String,
    pub seed_hex: String,
}

#[derive(Deserialize)]
pub struct Cases {
    pub version: u32,
    pub base: Input,
    pub valid: Vec<Case>,
    pub invalid: Vec<Rejection>,
}

pub fn decode_hex<const N: usize>(value: &str) -> Result<[u8; N], String> {
    if value.len() != N.saturating_mul(2) {
        return Err("invalid fixture hex length".to_owned());
    }
    let mut decoded = [0u8; N];
    for (target, pair) in decoded.iter_mut().zip(value.as_bytes().chunks_exact(2)) {
        let text = std::str::from_utf8(pair).map_err(|_| "invalid fixture hex")?;
        *target = u8::from_str_radix(text, 16).map_err(|_| "invalid fixture hex")?;
    }
    Ok(decoded)
}

pub fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::arithmetic_side_effects)]
    use super::*;
    use data_encoding::BASE64URL_NOPAD;
    use ed25519_dalek::{Signature, VerifyingKey};
    use pulsebeam_core::{
        auth::{
            ApiSigningKey, DEVELOPMENT_PROJECT_ID, KeyRegistry, KeyRegistryEntry, TokenError,
            authenticate_participant_token as verify_participant_token,
        },
        identity::{ApiKeyId, ParticipantId, ProjectId, RoomId},
    };

    #[test]
    fn shared_contract_and_server_authorization() {
        let vectors: Vectors = serde_json::from_str(include_str!("../vectors.json")).unwrap();
        let cases: Cases = serde_json::from_str(include_str!("../cases.json")).unwrap();
        assert_eq!(vectors.version, 2);
        assert_eq!(cases.version, vectors.version);
        assert!(cases.base == vectors.base);
        assert!(cases.invalid == vectors.invalid);
        assert_eq!(cases.valid.len(), vectors.valid.len());
        for (case, golden) in cases.valid.iter().zip(&vectors.valid) {
            assert_eq!(case.name, golden.name);
            assert!(case.input == golden.input);
            assert_eq!(case.seed_hex, golden.seed_hex);
            let input = &golden.input;
            let project: ProjectId = input.project_id.parse().unwrap();
            let key: ApiKeyId = input.key_id.parse().unwrap();
            let signing: ApiSigningKey = input.secret.parse().unwrap();
            assert_eq!(
                project.as_str(),
                golden.canonical_project_id,
                "{}",
                golden.name
            );
            assert_eq!(key.as_str(), golden.canonical_key_id);
            assert_eq!(
                signing,
                ApiSigningKey::from_seed(decode_hex(&golden.seed_hex).unwrap())
            );
            assert_eq!(
                encode_hex(signing.public_key().as_bytes()),
                golden.public_key_hex
            );
            // Anchor the independent verifier in RFC 8032 test 2, not derived SDK output.
            assert_eq!(
                golden.public_key_hex,
                "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c"
            );
            let independent =
                VerifyingKey::from_bytes(&decode_hex(&golden.public_key_hex).unwrap()).unwrap();
            let signature = BASE64URL_NOPAD.decode(golden.signature.as_bytes()).unwrap();
            independent
                .verify_strict(
                    golden.signing_input.as_bytes(),
                    &Signature::from_slice(&signature).unwrap(),
                )
                .unwrap();
            let signing_input = format!(
                "{}.{}",
                BASE64URL_NOPAD.encode(golden.header.as_bytes()),
                BASE64URL_NOPAD.encode(golden.claims.as_bytes())
            );
            assert_eq!(signing_input, golden.signing_input);
            assert_eq!(
                format!("{signing_input}.{}", golden.signature),
                golden.token
            );
            let exp: u64 = input.expiration.parse().unwrap();
            let api_key = format!(
                "{}.{}.{}",
                golden.canonical_project_id, golden.canonical_key_id, input.secret
            );
            let issue = || {
                pulsebeam_server::sign_participant_token(
                    &api_key,
                    &input.grants,
                    &input.participant,
                    exp,
                )
                .unwrap()
            };
            let issued = issue();
            assert_ne!(issued.jti, issue().jti);
            let parts: Vec<_> = issued.token.split('.').collect();
            let claims: serde_json::Value =
                serde_json::from_slice(&BASE64URL_NOPAD.decode(parts[1].as_bytes()).unwrap())
                    .unwrap();
            let mut expected: serde_json::Value = serde_json::from_str(&golden.claims).unwrap();
            assert_eq!(claims["jti"], issued.jti.to_string());
            expected["jti"] = claims["jti"].clone();
            assert_eq!(claims, expected);
            let signature = BASE64URL_NOPAD.decode(parts[2].as_bytes()).unwrap();
            independent
                .verify_strict(
                    format!("{}.{}", parts[0], parts[1]).as_bytes(),
                    &Signature::from_slice(&signature).unwrap(),
                )
                .unwrap();
            let registry = KeyRegistry::new(
                project,
                vec![KeyRegistryEntry {
                    key_id: key,
                    public_key: signing.public_key(),
                }],
            )
            .unwrap();
            if exp == 0 {
                assert_eq!(
                    verify_participant_token(&registry, &golden.token, 0),
                    Err(TokenError::Expired)
                );
                assert_eq!(
                    verify_participant_token(&registry, &golden.token, 1),
                    Err(TokenError::Expired)
                );
                continue;
            }
            let principal = verify_participant_token(&registry, &golden.token, exp - 1).unwrap();
            assert_eq!(principal.project_id(), project);
            assert_eq!(principal.participant_id().as_str(), input.participant);
            assert_eq!(principal.expiry().unix_seconds(), exp);
            let live = verify_participant_token(&registry, &issued.token, exp - 1).unwrap();
            assert_eq!(live.token_id().to_string(), issued.jti.to_string());
            let selected = principal.authorize_room(RoomId::new(&input.requested_room).unwrap());
            assert_eq!(selected.is_ok(), golden.name != "deny-all");
            if let Ok(authorization) = selected {
                assert_eq!(authorization.grants, input.grants);
                assert_eq!(
                    authorization.room_id.as_ref().unwrap().as_str(),
                    input.requested_room
                );
                assert_eq!(authorization.participant_id.as_str(), input.participant);
                assert_eq!(authorization.expiry.unix_seconds(), exp);
            }
            assert_eq!(
                verify_participant_token(&registry, &golden.token, exp),
                Err(TokenError::Expired)
            );
            if let Some(after) = exp.checked_add(1) {
                assert_eq!(
                    verify_participant_token(&registry, &golden.token, after),
                    Err(TokenError::Expired)
                );
            }
            let wrong_secret = KeyRegistry::new(
                project,
                vec![KeyRegistryEntry {
                    key_id: key,
                    public_key: ApiSigningKey::from_seed([7; 32]).public_key(),
                }],
            )
            .unwrap();
            assert_eq!(
                verify_participant_token(&wrong_secret, &golden.token, exp - 1),
                Err(TokenError::Invalid)
            );
            let wrong_project = KeyRegistry::new(
                *DEVELOPMENT_PROJECT_ID,
                vec![KeyRegistryEntry {
                    key_id: key,
                    public_key: signing.public_key(),
                }],
            )
            .unwrap();
            assert_eq!(
                verify_participant_token(&wrong_project, &golden.token, exp - 1),
                Err(TokenError::Invalid)
            );
            let wrong_key = KeyRegistry::new(
                project,
                vec![KeyRegistryEntry {
                    key_id: *pulsebeam_core::auth::DEVELOPMENT_API_KEY_ID,
                    public_key: signing.public_key(),
                }],
            )
            .unwrap();
            assert_eq!(
                verify_participant_token(&wrong_key, &golden.token, exp - 1),
                Err(TokenError::Invalid)
            );
            let tampered_claims = golden.claims.replacen(
                &format!(
                    "\"sub\":{}",
                    serde_json::to_string(&input.participant).unwrap()
                ),
                &format!(
                    "\"sub\":{}",
                    serde_json::to_string(&"x".repeat(input.participant.len())).unwrap()
                ),
                1,
            );
            let tampered = format!(
                "{}.{}.{}",
                BASE64URL_NOPAD.encode(golden.header.as_bytes()),
                BASE64URL_NOPAD.encode(tampered_claims.as_bytes()),
                golden.signature
            );
            assert_eq!(tampered.len(), golden.token.len());
            assert_eq!(
                verify_participant_token(&registry, &tampered, exp - 1),
                Err(TokenError::Invalid)
            );
            let mut signature = signature;
            *signature.first_mut().unwrap() ^= 1;
            let tampered = format!("{signing_input}.{}", BASE64URL_NOPAD.encode(&signature));
            assert_eq!(
                verify_participant_token(&registry, &tampered, exp - 1),
                Err(TokenError::Invalid)
            );
        }
        // Verify internal ID and signing-secret codec fixtures here. Packaged
        // SDK consumers exercise only the public opaque-key signing contract.
        for rejection in vectors.invalid {
            if rejection.field == "grants" {
                let grants = serde_json::from_value::<GrantSet>(rejection.value);
                if rejection.name == "encoded-token-over-limit" {
                    let error = pulsebeam_server::sign_participant_token(
                        &format!(
                            "{}.{}.{}",
                            vectors.base.project_id, vectors.base.key_id, vectors.base.secret
                        ),
                        &grants.unwrap(),
                        &vectors.base.participant,
                        vectors.base.expiration.parse().unwrap(),
                    )
                    .unwrap_err();
                    assert!(matches!(
                        error,
                        pulsebeam_server::SigningError::TokenTooLarge
                    ));
                } else {
                    assert!(grants.is_err(), "{}", rejection.name);
                }
                continue;
            }
            let Some(value) = rejection.value.as_str() else {
                continue;
            };
            let rejected = match rejection.field.as_str() {
                "project_id" => value.parse::<ProjectId>().is_err(),
                "key_id" => value.parse::<ApiKeyId>().is_err(),
                "secret" => value.parse::<ApiSigningKey>().is_err(),
                "participant" => pulsebeam_server::sign_participant_token(
                    &format!(
                        "{}.{}.{}",
                        vectors.base.project_id, vectors.base.key_id, vectors.base.secret
                    ),
                    &GrantSet::default(),
                    value,
                    2000,
                )
                .is_err(),
                "expiration" => continue,
                _ => panic!("unknown conformance field"),
            };
            assert!(rejected, "{}", rejection.name);
        }
    }
}
