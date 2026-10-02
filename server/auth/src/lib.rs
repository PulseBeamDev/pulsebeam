use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Input {
    pub project_id: String,
    pub key_id: String,
    pub secret: String,
    pub room: String,
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
            ApiSigningKey, DEVELOPMENT_PROJECT_ID, ProjectKey, ProjectKeys, ProjectRegistry,
            TokenError, verify_participant_token,
        },
        identity::{
            ApiKeyId, ParticipantExternalId, ParticipantId, ProjectId, RoomExternalId, RoomId,
        },
    };

    #[test]
    fn shared_contract_and_server_authorization() {
        let vectors: Vectors = serde_json::from_str(include_str!("../vectors.json")).unwrap();
        let cases: Cases = serde_json::from_str(include_str!("../cases.json")).unwrap();
        assert_eq!(vectors.version, 1);
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
                encode_hex(signing.verifying_key().as_bytes()),
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
            assert_eq!(
                pulsebeam_server::sign_participant_token(
                    &input.project_id,
                    &input.key_id,
                    &input.secret,
                    &input.room,
                    &input.participant,
                    exp
                )
                .unwrap(),
                golden.token,
                "{}",
                golden.name,
            );
            let registry = ProjectRegistry::new(vec![ProjectKeys {
                project_id: project,
                keys: vec![ProjectKey {
                    key_id: key,
                    verifying_key: signing.verifying_key(),
                }],
            }])
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
            let authorization =
                verify_participant_token(&registry, &golden.token, exp - 1).unwrap();
            assert_eq!(authorization.project_id, project);
            assert_eq!(authorization.room_external_id.as_str(), input.room);
            assert_eq!(
                authorization.participant_external_id.as_str(),
                input.participant
            );
            assert_eq!(authorization.expiry.unix_seconds(), exp);
            assert_eq!(
                authorization.room_id,
                RoomId::derive(&project, &authorization.room_external_id)
            );
            assert_eq!(
                authorization.participant_id,
                ParticipantId::derive(
                    &authorization.room_id,
                    &authorization.participant_external_id
                )
            );
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
            let wrong_secret = ProjectRegistry::new(vec![ProjectKeys {
                project_id: project,
                keys: vec![ProjectKey {
                    key_id: key,
                    verifying_key: ApiSigningKey::from_seed([7; 32]).verifying_key(),
                }],
            }])
            .unwrap();
            assert_eq!(
                verify_participant_token(&wrong_secret, &golden.token, exp - 1),
                Err(TokenError::Invalid)
            );
            let wrong_project = ProjectRegistry::new(vec![ProjectKeys {
                project_id: DEVELOPMENT_PROJECT_ID,
                keys: vec![ProjectKey {
                    key_id: key,
                    verifying_key: signing.verifying_key(),
                }],
            }])
            .unwrap();
            assert_eq!(
                verify_participant_token(&wrong_project, &golden.token, exp - 1),
                Err(TokenError::Invalid)
            );
            let wrong_key = ProjectRegistry::new(vec![ProjectKeys {
                project_id: project,
                keys: vec![ProjectKey {
                    key_id: pulsebeam_core::auth::DEVELOPMENT_API_KEY_ID,
                    verifying_key: signing.verifying_key(),
                }],
            }])
            .unwrap();
            assert_eq!(
                verify_participant_token(&wrong_key, &golden.token, exp - 1),
                Err(TokenError::Invalid)
            );
            let tampered_claims = golden
                .claims
                .replace(&input.participant, "OtherParticipant");
            let tampered = format!(
                "{}.{}.{}",
                BASE64URL_NOPAD.encode(golden.header.as_bytes()),
                BASE64URL_NOPAD.encode(tampered_claims.as_bytes()),
                golden.signature
            );
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
        // This loop checks server codec compatibility. The packaged Rust consumer
        // and compile-fail contract in :fast account for every SDK rejection.
        for rejection in vectors.invalid {
            let Some(value) = rejection.value.as_str() else {
                continue;
            };
            let rejected = match rejection.field.as_str() {
                "project_id" => value.parse::<ProjectId>().is_err(),
                "key_id" => value.parse::<ApiKeyId>().is_err(),
                "secret" => value.parse::<ApiSigningKey>().is_err(),
                "room" => RoomExternalId::new(value).is_err(),
                "participant" => ParticipantExternalId::new(value).is_err(),
                "expiration" => continue,
                _ => panic!("unknown conformance field"),
            };
            assert!(rejected, "{}", rejection.name);
        }
    }
}
