mod support;

use pulsebeam_core::{
    auth::mint_development_token,
    identity::{ParticipantExternalId, RoomExternalId},
};
use std::error::Error;
use std::path::PathBuf;
use support::{DestinationServer, StaticServer, TestResult, capabilities, evaluate_json, navigate};
use thirtyfour::prelude::WebDriver;
use thirtyfour::testing::run_browser_test;

#[tokio::test(flavor = "multi_thread")]
async fn meet_leave_releases_capture_and_transport() -> TestResult<()> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/meet/out");
    if !root.join("index.html").is_file() {
        return Err("build Meet through its owning test-slow gate first".into());
    }
    let _destination = DestinationServer::start()?;
    let server = StaticServer::start(root).await?;
    let url = server.url("");
    let token = mint_development_token(
        &RoomExternalId::new("meet-browser-contract")?,
        &ParticipantExternalId::new("meet-browser-participant")?,
        u64::MAX,
    )?;
    let contract = include_str!("../../../apps/meet/scripts/browser-contract.mjs")
        .replace("__TOKEN__", &token);
    run_browser_test(WebDriver::managed(capabilities()?), |driver| async move {
        let bidi = driver.bidi().await?;
        let context = bidi.browsing_context().top_level().await?;
        navigate(&bidi, &context, url).await?;
        let passed: bool = evaluate_json(&bidi, &context, &contract).await?;
        assert!(passed, "Meet runtime contract did not finish");
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    })
    .await?;
    assert!(
        server.wasm_requests() > 0,
        "Meet must load the real Web runtime"
    );
    Ok(())
}
