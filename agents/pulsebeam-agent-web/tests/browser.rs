#[path = "support/destination.rs"]
mod destination;
mod support;

use destination::DestinationServer;
use pulsebeam_core::identity::{ParticipantExternalId, RoomExternalId};
use std::path::PathBuf;
use support::{
    StaticServer, TestResult, capabilities, evaluate_json, managed_driver, mint_development_token,
    navigate,
};
use thirtyfour::{prelude::ChromiumLikeCapabilities, testing::run_browser_test};

const LOAD: &str = include_str!("contracts/load-web.js");

fn root() -> PathBuf {
    std::env::var_os("PULSEBEAM_WEB_FIXTURE_ROOT")
        .expect("declared SDK browser assets")
        .into()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn native_platform_smoke() -> TestResult<()> {
    let destination = DestinationServer::start().await?;
    let server = StaticServer::start(root()).await?;
    let room = RoomExternalId::new("public-web-contract")?;
    let sender =
        mint_development_token(&room, &ParticipantExternalId::new("web-sender")?, u64::MAX)?;
    let receiver = mint_development_token(
        &room,
        &ParticipantExternalId::new("web-receiver")?,
        u64::MAX,
    )?;
    let live = include_str!("contracts/live-agent-contract.js")
        .replace("__DESTINATION_ENDPOINT__", &destination.endpoint())
        .replace("__SENDER_TOKEN__", &sender)
        .replace("__RECEIVER_TOKEN__", &receiver);
    let mut browser = capabilities()?;
    browser.add_arg("--autoplay-policy=no-user-gesture-required")?;
    browser.add_arg("--force-device-scale-factor=2")?;
    let fixture = &server;
    run_browser_test(managed_driver(browser), |driver| async move {
        let bidi = driver.bidi().await?;
        let context = bidi.browsing_context().top_level().await?;
        for (name, script, report) in [
            (
                "physical-layout",
                include_str!("contracts/remote-catalog-contract.js"),
                true,
            ),
            (
                "decoded-audio-replacement",
                include_str!("contracts/remote-audio-contract.js"),
                false,
            ),
            ("connected-media", live.as_str(), false),
        ] {
            // Full navigation gives every case fresh globals, module state and native resources.
            navigate(
                &bidi,
                &context,
                fixture.url(&format!("tests/fixture.html?case={name}")),
            )
            .await?;
            let _: () = evaluate_json(&bidi, &context, LOAD).await?;
            let expression = if report {
                format!(
                    "({}).then(result => Object.values(result).every(value => value === true))",
                    script.trim().trim_end_matches(';')
                )
            } else {
                script.to_owned()
            };
            let passed: bool = evaluate_json(&bidi, &context, &expression)
                .await
                .map_err(|error| format!("{name}: {error}"))?;
            assert!(passed, "{name}");
            let rejections: usize = evaluate_json(
                &bidi,
                &context,
                include_str!("contracts/unhandled-rejections.js"),
            )
            .await?;
            assert_eq!(rejections, 0, "{name}: unhandled completion");
        }
        navigate(&bidi, &context, fixture.url("tests/uniffi-fixture.html")).await?;
        let ffi: serde_json::Value = evaluate_json(
            &bidi,
            &context,
            include_str!("contracts/uniffi-media-contract.js"),
        )
        .await?;
        for field in ["trackIdentity", "streamIdentity"] {
            assert_eq!(ffi[field], true, "generated real media handoff: {field}");
        }
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await?;
    assert!(
        server.wasm_requests() > 0,
        "packaged WASM was actually loaded"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn gesture_before_media_smoke() -> TestResult<()> {
    let server = StaticServer::start(root()).await?;
    let mut browser = capabilities()?;
    browser.add_arg("--autoplay-policy=document-user-activation-required")?;
    run_browser_test(managed_driver(browser), |driver| async move {
        let bidi = driver.bidi().await?;
        let context = bidi.browsing_context().top_level().await?;
        navigate(&bidi, &context, server.url("tests/fixture.html")).await?;
        let _: () = evaluate_json(&bidi, &context, LOAD).await?;
        let started: bool = evaluate_json(
            &bidi,
            &context,
            include_str!("contracts/early-audio-start.js"),
        )
        .await?;
        assert!(started);
        driver
            .find(thirtyfour::By::Id("early-audio"))
            .await?
            .click()
            .await?;
        let passed: bool = evaluate_json(
            &bidi,
            &context,
            include_str!("contracts/early-audio-finish.js"),
        )
        .await?;
        assert!(passed);
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn gesture_required_audio_smoke() -> TestResult<()> {
    let server = StaticServer::start(root()).await?;
    let mut browser = capabilities()?;
    browser.add_arg("--autoplay-policy=document-user-activation-required")?;
    run_browser_test(managed_driver(browser), |driver| async move {
        let bidi = driver.bidi().await?;
        let context = bidi.browsing_context().top_level().await?;
        navigate(&bidi, &context, server.url("tests/fixture.html")).await?;
        let _: () = evaluate_json(&bidi, &context, LOAD).await?;
        let started: bool = evaluate_json(
            &bidi,
            &context,
            include_str!("contracts/remote-audio-recovery-start.js"),
        )
        .await?;
        assert!(started);
        driver
            .find(thirtyfour::By::Id("resume-audio"))
            .await?
            .click()
            .await?;
        let passed: bool = evaluate_json(
            &bidi,
            &context,
            include_str!("contracts/remote-audio-recovery-finish.js"),
        )
        .await?;
        assert!(passed);
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn web_adapter_restart_smoke() -> TestResult<()> {
    let destination = DestinationServer::start().await?;
    let address = destination.address();
    let server = StaticServer::start(root()).await?;
    let room = RoomExternalId::new("web-adapter-restart")?;
    let sender =
        mint_development_token(&room, &ParticipantExternalId::new("publisher")?, u64::MAX)?;
    let receiver =
        mint_development_token(&room, &ParticipantExternalId::new("subscriber")?, u64::MAX)?;
    let start = include_str!("contracts/reliable-restart-start.js")
        .replace("__DESTINATION_ENDPOINT__", &destination.endpoint())
        .replace("__SENDER_TOKEN__", &sender)
        .replace("__RECEIVER_TOKEN__", &receiver);
    run_browser_test(managed_driver(capabilities()?), |driver| async move {
        let bidi = driver.bidi().await?;
        let context = bidi.browsing_context().top_level().await?;
        navigate(&bidi, &context, server.url("tests/fixture.html")).await?;
        let _: () = evaluate_json(&bidi, &context, LOAD).await?;
        let before: serde_json::Value = evaluate_json(&bidi, &context, &start).await?;
        assert_eq!(before["initial"], serde_json::json!([1]));
        drop(destination);
        let _restarted = DestinationServer::bind(address).await?;
        let recovered: bool = evaluate_json(
            &bidi,
            &context,
            include_str!("contracts/reliable-restart-finish.js"),
        )
        .await?;
        assert!(recovered);
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await?;
    Ok(())
}
