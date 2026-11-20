use pulsebeam_server::{GrantSet, SignedParticipantToken, SigningError, sign_participant_token};
use serde::Deserialize;

const FIXTURE_API_KEY: &str = "p_0067MYZ1001R010000000000064.kid_0067MYZ1005R030010000000068.MC4CAQAwBQYDK2VwBCIEIEzNCJso_5banbbDRuwRTg9bijGfNaumJNqM9u1PuKb7";

#[derive(Deserialize)]
struct Input {
    grants: GrantSet,
    participant: String,
    expiration: String,
}

#[derive(Deserialize)]
struct Golden {
    name: String,
    input: Input,
}

#[derive(Deserialize)]
struct Rejection {
    name: String,
    value: serde_json::Value,
}

#[derive(Deserialize)]
struct Vectors {
    base: Input,
    valid: Vec<Golden>,
    invalid: Vec<Rejection>,
}

fn sign(input: &Input) -> Result<SignedParticipantToken, SigningError> {
    sign_participant_token(
        FIXTURE_API_KEY,
        &input.grants,
        &input.participant,
        input.expiration.parse().unwrap(),
    )
}

fn grants(pattern: &str) -> GrantSet {
    serde_json::from_value(serde_json::json!([{"rooms": [pattern], "allow": ["join"]}])).unwrap()
}

#[test]
fn public_artifact_conformance_and_diagnostics() {
    let vectors: Vectors = serde_json::from_str(include_str!("../../auth/vectors.json")).unwrap();
    for golden in vectors.valid {
        let issued = sign(&golden.input).unwrap();
        assert_eq!(issued.jti.get_version_num(), 4);
        assert_ne!(issued.jti, sign(&golden.input).unwrap().jti);
        assert!(!format!("{issued:?}").contains(&issued.token));
        if golden.name == "encoded-token-limit" {
            assert_eq!(issued.token.len(), 8192);
        }
    }
    let oversized = vectors
        .invalid
        .into_iter()
        .find(|row| row.name == "encoded-token-over-limit")
        .unwrap();
    let error = sign_participant_token(
        FIXTURE_API_KEY,
        &serde_json::from_value(oversized.value).unwrap(),
        &vectors.base.participant,
        vectors.base.expiration.parse().unwrap(),
    )
    .unwrap_err();
    assert!(matches!(error, SigningError::TokenTooLarge));
    assert!(!format!("{error:?}").contains(FIXTURE_API_KEY));
    for invalid in ["", "bad\nsubject", &"é".repeat(129)] {
        assert!(matches!(
            sign_participant_token(FIXTURE_API_KEY, &GrantSet::default(), invalid, 2000),
            Err(SigningError::Participant(_))
        ));
    }
    let pieces: Vec<&str> = FIXTURE_API_KEY.split('.').collect();
    let arbitrary = format!("unknown-issuer.unknown-kid.{}", pieces[2]);
    assert!(
        sign_participant_token(
            &arbitrary,
            &grants("room with spaces"),
            "Participant_With-MixedCase",
            2000
        )
        .is_ok()
    );
}

#[test]
fn packaged_development_api_key_matches_canonical_json() {
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("../../auth/development.json")).unwrap();
    let api_key = pulsebeam_server::insecure_dev_api_key().unwrap();
    assert_eq!(api_key, expected["apiKey"].as_str().unwrap());
    assert!(
        !sign_participant_token(&api_key, &grants("demo"), "alice", 2_000_000_000)
            .unwrap()
            .token
            .is_empty()
    );
    for invalid in [
        api_key.replacen("MC4CAQ", "invalid", 1),
        api_key[..api_key.len() - 1].to_string(),
        format!("{api_key}.extra"),
    ] {
        assert!(sign_participant_token(&invalid, &grants("demo"), "alice", 2_000_000_000).is_err());
    }
}
