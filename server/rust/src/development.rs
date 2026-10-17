//! Publicly known API key used only by PulseBeam servers running with --dev.
use serde::Deserialize;

#[derive(Deserialize)]
struct Development {
    #[serde(rename = "apiKey")]
    api_key: String,
}

// Bazel generates this file beside the source; published packages bundle it
// at the same relative path.
const DEVELOPMENT_JSON: &str = include_str!("development.json");

/// Return the publicly known API key for local --dev servers only.
/// Never use this with real users or an internet-facing server.
pub fn insecure_dev_api_key() -> Result<String, serde_json::Error> {
    let development: Development = serde_json::from_str(DEVELOPMENT_JSON)?;
    Ok(development.api_key)
}
