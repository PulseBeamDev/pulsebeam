use pulsebeam_server::sign_participant_token;
use serde::Deserialize;

#[derive(Clone, Deserialize)]
struct Input {
    project_id: String,
    key_id: String,
    secret: String,
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
struct Rejection {
    name: String,
    field: String,
    value: serde_json::Value,
    representation: Option<String>,
}

#[derive(Deserialize)]
struct Vectors {
    base: Input,
    valid: Vec<Golden>,
    invalid: Vec<Rejection>,
}

fn sign(input: &Input) -> Result<String, pulsebeam_server::SigningError> {
    sign_participant_token(
        &input.project_id,
        &input.key_id,
        &input.secret,
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
    for rejection in vectors.invalid {
        assert!(
            [
                "project_id",
                "key_id",
                "secret",
                "room",
                "participant",
                "expiration"
            ]
            .contains(&rejection.field.as_str())
        );
        let mut input = vectors.base.clone();
        match (rejection.field.as_str(), rejection.value.as_str()) {
            ("expiration", _) => {
                // The public compiler contract covers mandatory u64, not f64,
                // bool, string, negative constants or overflowing constants.
                match rejection.representation.as_deref() {
                    Some("number") | Some("string") => assert!(rejection.value.is_string()),
                    None => assert!(rejection.value.is_null() || rejection.value.is_boolean()),
                    _ => panic!("unhandled expiration representation"),
                }
                continue;
            }
            (_, None) => {
                // Compiler contract covers omitted arguments and non-string inputs.
                assert!(
                    rejection.value.is_null()
                        || rejection.value.is_boolean()
                        || rejection.value.is_number()
                );
                continue;
            }
            ("project_id", Some(value)) => input.project_id = value.to_owned(),
            ("key_id", Some(value)) => input.key_id = value.to_owned(),
            ("secret", Some(value)) => input.secret = value.to_owned(),
            ("room", Some(value)) => input.room = value.to_owned(),
            ("participant", Some(value)) => input.participant = value.to_owned(),
            _ => panic!("unhandled conformance input"),
        }
        let error = sign(&input).expect_err(&rejection.name);
        let diagnostic = format!("{error} {error:?}");
        if !input.secret.is_empty() {
            assert!(!diagnostic.contains(&input.secret), "{}", rejection.name);
        }
        assert!(!diagnostic.contains("4ccd089b28ff96da"));
    }
}

#[test]
fn packaged_development_credentials_match_canonical_json() {
    let expected: serde_json::Value =
        serde_json::from_str(include_str!("../../auth/development.json")).unwrap();
    let actual = pulsebeam_server::insecure_dev_credentials();
    assert_eq!(actual.project_id, expected["projectId"].as_str().unwrap());
    assert_eq!(actual.key_id, expected["keyId"].as_str().unwrap());
    assert_eq!(actual.secret, expected["secret"].as_str().unwrap());
    assert_eq!(actual.public_key, expected["publicKey"].as_str().unwrap());
    let token = pulsebeam_server::sign_participant_token(
        &actual.project_id, &actual.key_id, &actual.secret, "demo", "alice", 2_000_000_000,
    ).unwrap();
    assert!(!token.is_empty());
}
