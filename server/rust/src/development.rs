//! Publicly known credentials used only by PulseBeam servers running with --dev.
use serde::Deserialize;

/// Insecure, fixed development credentials. Never use to authorize real users.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InsecureDevCredentials {
    pub api_key: String,
    pub project_id: String,
    pub key_id: String,
    pub secret: String,
    pub public_key: String,
}

// Bazel generates this file beside the source; published packages bundle it
// at the same relative path.
const DEVELOPMENT_JSON: &str = include_str!("development.json");

/// The same embedded development identity used by the SFU's --dev mode.
pub fn insecure_dev_credentials() -> Result<InsecureDevCredentials, serde_json::Error> {
    serde_json::from_str(DEVELOPMENT_JSON)
}
