//! Publicly known credentials used only by PulseBeam servers running with --dev.
#![allow(unexpected_cfgs)] // Bazel uses a compile-time file path; Cargo uses the packaged file.
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

// Bazel's generated file lives under bazel-out, not next to this source file.
// Packaged Cargo consumers use the normal relative include.
#[cfg(pulsebeam_bazel)]
const DEVELOPMENT_JSON: &str =
    include_str!(concat!("../../../", env!("PULSEBEAM_DEVELOPMENT_JSON")));
#[cfg(not(pulsebeam_bazel))]
const DEVELOPMENT_JSON: &str = include_str!("development.json");

/// The same embedded development identity used by the SFU's --dev mode.
pub fn insecure_dev_credentials() -> Result<InsecureDevCredentials, serde_json::Error> {
    serde_json::from_str(DEVELOPMENT_JSON)
}
