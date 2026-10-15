mod support;

use serde::Deserialize;
use std::{error::Error, path::PathBuf};
use support::{StaticServer, TestResult, capabilities, evaluate_json, managed_driver, navigate};
use thirtyfour::testing::run_browser_test;

const REACT: &str = include_str!("../../react/tests/browser/observe.js");

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct React {
    removed_legacy_surface: bool,
    playback_retained: bool,
    playback_latest_callback: bool,
    capture_devices: bool,
    committed_render_isolation: bool,
    autoplay_respected: bool,
    capture_replacement: bool,
    capture_fencing: bool,
    capture_display: bool,
    capture_errors: bool,
    capture_session: bool,
    owned_independent: bool,
    owned_renewal: bool,
    owned_replacement: bool,
    owned_cleanup: bool,
    local_preview: bool,
    captured_preview: bool,
    playback_probe_error: String,
    replaced: bool,
    playback_error: bool,
    detached: bool,
    audio_without_ui: bool,
}
#[tokio::test(flavor = "multi_thread")]
async fn react_provider_contract_runs_through_bidi() -> TestResult<()> {
    let fixture = PathBuf::from(std::env::var_os("PULSEBEAM_REACT_FIXTURE_ROOT")
        .ok_or("missing declared React fixture; run //agents/pulsebeam-agent-web:browser through ./bazel test")?);
    if !fixture.join("index.html").is_file() {
        return Err("declared React browser fixture is missing".into());
    }
    let server = StaticServer::start(fixture).await?;
    let url = server.url("index.html");
    run_browser_test(managed_driver(capabilities()?), |driver| async move {
        let bidi = driver.bidi().await?;
        let context = bidi.browsing_context().top_level().await?;
        navigate(&bidi, &context, url).await?;
        let r: React = evaluate_json(&bidi, &context, REACT).await?;
        assert!(
            r.removed_legacy_surface
                && r.playback_retained
                && r.playback_latest_callback
                && r.capture_devices
                && r.committed_render_isolation
                && r.autoplay_respected
                && r.capture_replacement
                && r.capture_fencing
                && r.capture_display
                && r.capture_errors
                && r.capture_session
                && r.owned_independent
                && r.owned_renewal
                && r.owned_replacement
                && r.owned_cleanup
                && r.local_preview
                && r.captured_preview
                && r.playback_probe_error.is_empty()
                && r.replaced
                && r.playback_error
                && r.detached
                && r.audio_without_ui,
            "React browser contract result: {r:?}"
        );
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    })
    .await
    .map_err(|e| format!("React browser contract failed: {e}").into())
}
