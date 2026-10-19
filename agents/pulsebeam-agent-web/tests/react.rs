mod support;

use support::{StaticServer, TestResult, capabilities, evaluate_json, managed_driver, navigate};
use thirtyfour::testing::run_browser_test;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn react_subscription_capture_and_owner_smoke() -> TestResult<()> {
    let fixture = std::env::var("PULSEBEAM_REACT_FIXTURE_ROOT")?;
    let server = StaticServer::start(fixture).await?;
    run_browser_test(managed_driver(capabilities()?), |driver| async move {
        let bidi = driver.bidi().await?;
        let context = bidi.browsing_context().top_level().await?;
        navigate(&bidi, &context, server.url("index.html")).await?;
        let passed: bool = evaluate_json(
            &bidi,
            &context,
            include_str!("../../react/tests/browser/observe.js"),
        )
        .await?;
        assert!(passed, "React subscription/render/update/cleanup");
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await?;
    Ok(())
}
