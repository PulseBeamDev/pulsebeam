//! Publicly known credentials used only by PulseBeam servers running with --dev.
use serde::Deserialize;

/// Insecure, fixed development credentials. Never use to authorize real users.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InsecureDevCredentials {
    pub project_id: String,
    pub key_id: String,
    pub secret: String,
    pub public_key: String,
}

/// The same embedded development identity used by the SFU's --dev mode.
pub fn insecure_dev_credentials() -> InsecureDevCredentials {
    serde_json::from_str(include_str!("development.json"))
        .expect("invalid embedded development credentials")
}
