//! Native server-side participant-token signing.
//!
//! API keys and absolute Unix expiration are always explicit. No clock,
//! environment lookup, service call, or registration check is performed.
//! Keep API keys on your server and send only the token to clients.

pub mod development;
pub use development::insecure_dev_api_key;

use data_encoding::BASE64URL_NOPAD;
use pulsebeam_auth::api_key::ApiKeyError;
use pulsebeam_auth::keys::ApiSigningKey;
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SigningError {
    #[error("invalid API key: {0}")]
    ApiKey(#[from] ApiKeyError),
    #[error("could not encode participant token")]
    Encoding(#[from] serde_json::Error),
}

#[derive(Serialize)]
struct Header<'a> {
    alg: &'a str,
    kid: &'a str,
    typ: &'a str,
}

#[derive(Serialize)]
struct Claims<'a> {
    iss: &'a str,
    aud: &'a str,
    sub: &'a str,
    room: &'a str,
    exp: u64,
}

/// Sign a participant JWT using one opaque server-only API key.
///
/// `expiration` is exact unsigned absolute Unix seconds, not a TTL.
/// Zero and past expiration may be signed; the server rejects at expiration.
/// External IDs retain their case; no clock, environment or network is used.
///
/// ```compile_fail
/// // Expiration is mandatory.
/// pulsebeam_server::sign_participant_token("key", "room", "participant");
/// ```
/// ```compile_fail
/// pulsebeam_server::sign_participant_token("key", "room", "participant", 1.5_f64);
/// ```
/// ```compile_fail
/// pulsebeam_server::sign_participant_token("key", "room", "participant", "2000");
/// ```
/// ```compile_fail
/// pulsebeam_server::sign_participant_token("key", "room", "participant", true);
/// ```
/// ```compile_fail
/// pulsebeam_server::sign_participant_token("key", "room", "participant", -1);
/// ```
/// ```compile_fail
/// pulsebeam_server::sign_participant_token("key", "room", "participant", 18446744073709551616);
/// ```
/// ```compile_fail
/// pulsebeam_server::sign_participant_token(true, "room", "participant", 2000);
/// ```
pub fn sign_participant_token(
    api_key: &str,
    room: &str,
    participant: &str,
    expiration: u64,
) -> Result<String, SigningError> {
    let mut parts = api_key.split('.');
    let (Some(project), Some(kid), Some(secret), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(ApiKeyError::InvalidFormat.into());
    };
    let signing_key: ApiSigningKey = secret.parse().map_err(|_| ApiKeyError::InvalidSecret)?;
    let header = serde_json::to_vec(&Header {
        alg: "EdDSA",
        kid,
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
