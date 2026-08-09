//! Deterministic token fixtures.
//!
//! The keypair is pinned rather than generated so no scenario touches OS entropy and any failure
//! reproduces exactly. These are test keys and are meant to be public.

use pulsebeam::control::auth::{JwtAlg, JwtKeyBytes};

pub const KID: &str = "sim-key";
pub const AUDIENCE: &str = "pulsebeam-sim";
pub const ISSUER: &str = "https://app.sim.test";
pub const RESUME_KID: &str = "sim-resume";

/// Cluster-wide resume secret. Fixed so a restarted node still verifies tokens it minted before,
/// which is the property `resume_across_node_restart_test` exists to prove.
pub const RESUME_SECRET: [u8; 32] = [0x5a; 32];

/// pkcs8 wrapper around a fixed Ed25519 seed.
fn pkcs8() -> Vec<u8> {
    let mut der = vec![
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];
    der.extend_from_slice(&[7u8; 32]);
    der
}

/// Public half of the seed above.
pub const PUBLIC_KEY: [u8; 32] = [
    234, 74, 108, 99, 226, 156, 82, 10, 190, 245, 80, 123, 19, 46, 197, 249, 149, 71, 118, 174,
    190, 190, 123, 146, 66, 30, 234, 105, 20, 70, 210, 44,
];

pub fn verifying_key() -> JwtKeyBytes {
    JwtKeyBytes::Ed25519Raw(PUBLIC_KEY.to_vec())
}

pub fn alg() -> JwtAlg {
    JwtAlg::Ed25519
}

#[derive(serde::Serialize)]
struct PbClaims {
    room: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    publish: bool,
    subscribe: bool,
}

#[derive(serde::Serialize)]
struct Claims {
    iss: String,
    sub: String,
    aud: String,
    exp: i64,
    iat: i64,
    jti: String,
    pb: PbClaims,
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Mint an access token for a room. `subject` is the end-user identity a resume is bound to.
pub fn mint_access_token(room: &str, subject: &str) -> String {
    mint_access_token_with(room, subject, true, true)
}

pub fn mint_access_token_with(room: &str, subject: &str, publish: bool, subscribe: bool) -> String {
    let issued = now();
    let claims = Claims {
        iss: ISSUER.to_string(),
        sub: subject.to_string(),
        aud: AUDIENCE.to_string(),
        exp: issued + 3600,
        iat: issued,
        jti: format!("sim-{subject}-{issued}"),
        pb: PbClaims {
            room: room.to_string(),
            name: Some(subject.to_string()),
            publish,
            subscribe,
        },
    };

    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::EdDSA);
    header.kid = Some(KID.to_string());
    jsonwebtoken::encode(
        &header,
        &claims,
        &jsonwebtoken::EncodingKey::from_ed_der(&pkcs8()),
    )
    .expect("pinned key must sign")
}
