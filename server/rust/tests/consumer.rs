use pulsebeam_server::sign_participant_token;
use serde::Deserialize;

const FIXTURE_API_KEY: &str = "p_0067MYZ1001R010000000000064.kid_0067MYZ1005R030010000000068.MC4CAQAwBQYDK2VwBCIEIEzNCJso_5banbbDRuwRTg9bijGfNaumJNqM9u1PuKb7";

#[derive(Deserialize)]
struct Input {
    room: String,
    participant: String,
    expiration: String,
}

#[derive(Deserialize)]
struct Golden {
    name: String,
    input: Input,
    token: String,
}

#[derive(Deserialize)]
struct Vectors {
    valid: Vec<Golden>,
}

fn sign(input: &Input) -> Result<String, pulsebeam_server::SigningError> {
    sign_participant_token(
        FIXTURE_API_KEY,
        &input.room,
        &input.participant,
        input.expiration.parse().unwrap(),
    )
}

#[test]
fn public_artifact_conformance_and_diagnostics() {
    let vectors: Vectors = serde_json::from_str(include_str!("../../auth/vectors.json")).unwrap();
    for golden in vectors.valid {
        assert_eq!(
            sign(&golden.input).unwrap(),
            golden.token,
            "{}",
            golden.name
        );
    }
    // The server, not the signer, validates issuer, kid and external IDs.
    let pieces: Vec<&str> = FIXTURE_API_KEY.split('.').collect();
    let arbitrary = format!("unknown-issuer.unknown-kid.{}", pieces[2]);
    let result = sign_participant_token(
        &arbitrary,
        "room with spaces",
        "participant/with/slashes",
        2000,
    );
    assert!(result.is_ok());
}

#[test]
fn packaged_development_api_key_matches_canonical_json() {
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("../../auth/development.json")).unwrap();
    let api_key = pulsebeam_server::insecure_dev_api_key().unwrap();
    assert_eq!(api_key, expected["apiKey"].as_str().unwrap());
    let token = sign_participant_token(&api_key, "demo", "alice", 2_000_000_000).unwrap();
    assert!(!token.is_empty());
    for invalid in [
        api_key.replacen("MC4CAQ", "invalid", 1),
        api_key[..api_key.len() - 1].to_string(),
        format!("{api_key}.extra"),
    ] {
        assert!(sign_participant_token(&invalid, "demo", "alice", 2_000_000_000).is_err());
    }
}
