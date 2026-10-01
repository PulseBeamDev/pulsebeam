//! Native server-side participant-token signing.
//!
//! Credentials and absolute Unix expiration are always explicit. No clock,
//! environment lookup, service call, or registration check is performed.
//! Keep signing secrets on your server and send only the token to clients.

use data_encoding::BASE64URL_NOPAD;
use pulsebeam_auth::identity::{IdValidationError, format_id, parse_id, validate_external};
use pulsebeam_auth::keys::{ApiSigningKey, KeyValidationError};
use serde::Serialize;
use uuid::Version;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SigningError {
    #[error("invalid {field}: {source}")]
    Identity {
        field: &'static str,
        source: IdValidationError,
    },
    #[error("invalid signing secret: {0}")]
    Secret(#[from] KeyValidationError),
    #[error("could not encode participant token")]
    Encoding(#[from] serde_json::Error),
}

#[derive(Serialize)]
struct Header<'a> {
    alg: &'a str,
    kid: String,
    typ: &'a str,
}

#[derive(Serialize)]
struct Claims<'a> {
    iss: String,
    aud: &'a str,
    sub: &'a str,
    room: &'a str,
    exp: u64,
}

fn canonical_id(value: &str, prefix: &'static str) -> Result<String, SigningError> {
    parse_id(value, prefix, Version::SortRand)
        .map(|uuid| format_id(prefix, uuid))
        .map_err(|source| SigningError::Identity {
            field: if prefix == "p" {
                "project ID"
            } else {
                "API key ID"
            },
            source,
        })
}

/// Sign a participant JWT with six mandatory inputs.
///
/// `expiration` is exact unsigned absolute Unix seconds, not a TTL. Zero and
/// past expiration may be signed; the server rejects at and after expiration.
/// Credential aliases are canonicalized, but external IDs retain their case.
/// Errors and default diagnostics do not include credentials or seed bytes.
///
/// ```compile_fail
/// // Expiration is mandatory.
/// pulsebeam_server::sign_participant_token("p", "kid", "sk", "room", "participant");
/// ```
/// ```compile_fail
/// pulsebeam_server::sign_participant_token("p", "kid", "sk", "room", "participant", 1.5_f64);
/// ```
/// ```compile_fail
/// pulsebeam_server::sign_participant_token("p", "kid", "sk", "room", "participant", "2000");
/// ```
/// ```compile_fail
/// pulsebeam_server::sign_participant_token("p", "kid", "sk", "room", "participant", true);
/// ```
/// ```compile_fail
/// pulsebeam_server::sign_participant_token("p", "kid", "sk", "room", "participant", -1);
/// ```
/// ```compile_fail
/// #![deny(overflowing_literals)]
/// pulsebeam_server::sign_participant_token("p", "kid", "sk", "room", "participant", 18446744073709551616);
/// ```
/// ```compile_fail
/// // Credentials and external IDs must be string references.
/// pulsebeam_server::sign_participant_token(true, "kid", "sk", "room", "participant", 2000);
/// ```
/// ```compile_fail
/// pulsebeam_server::sign_participant_token(42, "kid", "sk", "room", "participant", 2000);
/// ```
pub fn sign_participant_token(
    project_id: &str,
    key_id: &str,
    secret: &str,
    room: &str,
    participant: &str,
    expiration: u64,
) -> Result<String, SigningError> {
    let project = canonical_id(project_id, "p")?;
    let key = canonical_id(key_id, "kid")?;
    let signing_key: ApiSigningKey = secret.parse()?;
    for (field, value) in [("room", room), ("participant", participant)] {
        validate_external(value).map_err(|source| SigningError::Identity { field, source })?;
    }
    let header = serde_json::to_vec(&Header {
        alg: "EdDSA",
        kid: key,
        typ: "pb+jwt",
    })?;
    let claims = serde_json::to_vec(&Claims {
        iss: project,
        aud: "pb",
        sub: participant,
        room,
        exp: expiration,
    })?;
    let signing_input = format!(
        "{}.{}",
        BASE64URL_NOPAD.encode(&header),
        BASE64URL_NOPAD.encode(&claims)
    );
    let signature = signing_key.sign_message(signing_input.as_bytes());
    Ok(format!(
        "{signing_input}.{}",
        BASE64URL_NOPAD.encode(&signature)
    ))
}
