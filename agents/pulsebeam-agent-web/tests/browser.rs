#[path = "support/destination.rs"]
mod destination;
mod support;

use destination::DestinationServer;
use pulsebeam_core::identity::{ParticipantId, RoomId};
use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};
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

fn authorization_expiry() -> TestResult<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)?
        .as_secs()
        .checked_add(86_400)
        .ok_or_else(|| "test authorization expiry overflow".into())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn native_platform_smoke() -> TestResult<()> {
    let destination = DestinationServer::start().await?;
    let server = StaticServer::start(root()).await?;
    let room = RoomId::new("public-web-contract")?;
    let expires_at = authorization_expiry()?;
    let sender = mint_development_token(&room, &ParticipantId::new("web-sender")?, expires_at)?;
    let receiver = mint_development_token(&room, &ParticipantId::new("web-receiver")?, expires_at)?;
    let renewed_sender = mint_development_token(
        &room,
        &ParticipantId::new("web-sender")?,
        expires_at
            .checked_add(60)
            .ok_or("renewal expiry overflow")?,
    )?;
    let catalog = pulsebeam_server::sign_participant_token(
        &pulsebeam_server::insecure_dev_api_key().unwrap(),
        &serde_json::from_value(serde_json::json!([{
            "rooms": [room.as_str()], "allow": ["join", "discover"]
        }]))?,
        "web-catalog-observer",
        expires_at,
    )?;
    let live = include_str!("contracts/live-agent-contract.js")
        .replace("__DESTINATION_ENDPOINT__", &destination.endpoint())
        .replace("__SENDER_TOKEN__", &sender)
        .replace("__RECEIVER_TOKEN__", &receiver)
        .replace("__CATALOG_TOKEN__", &catalog.token)
        .replace("__RENEWED_SENDER_TOKEN__", &renewed_sender);
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
        let source_room = RoomId::new("react-native-source")?;
        let publisher_token = mint_development_token(
            &source_room,
            &ParticipantId::new("native-publisher")?,
            expires_at,
        )?;
        let receiver_token = pulsebeam_server::sign_participant_token(
            &pulsebeam_server::insecure_dev_api_key()?,
            &serde_json::from_value(serde_json::json!([
                {"rooms":["react-native-receiver"],"allow":["join"]},
                {"rooms":[source_room.as_str()],"allow":["subscribe"]}
            ]))?,
            "react-receiver",
            expires_at,
        )?;
        let catalog_token = pulsebeam_server::sign_participant_token(
            &pulsebeam_server::insecure_dev_api_key()?,
            &serde_json::from_value(serde_json::json!([
                {"rooms":[source_room.as_str()],"allow":["join", "discover"]}
            ]))?,
            "react-catalog-observer",
            expires_at,
        )?;
        let metadata_token = pulsebeam_server::sign_participant_token(
            &pulsebeam_server::insecure_dev_api_key()?,
            &serde_json::from_value(serde_json::json!([
                {"rooms":[source_room.as_str()],"allow":["join", "publish"]}
            ]))?,
            "react-metadata-publisher",
            expires_at,
        )?;
        let react = StaticServer::start(
            std::env::var_os("PULSEBEAM_REACT_FIXTURE_ROOT")
                .ok_or("missing React fixture assets")?,
        )
        .await?;
        let generated_react = StaticServer::start(
            std::env::var_os("PULSEBEAM_GENERATED_REACT_FIXTURE_ROOT")
                .ok_or("missing generated React fixture assets")?,
        )
        .await?;
        let mut producer = tokio::process::Command::new(
            std::env::var_os("PULSEBEAM_FOREIGN_SMOKE").ok_or("missing Python producer fixture")?,
        )
        .arg("--publish")
        .arg(destination.endpoint())
        .arg(publisher_token)
        .arg(source_room.as_str())
        .env_remove("LD_LIBRARY_PATH")
        .env_remove("LD_PRELOAD")
        .stdin(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
        let config = serde_json::json!({
            "endpoint": destination.endpoint(),
            "token": receiver_token.token,
            "room": "react-native-receiver",
            "sourceRoom": source_room.as_str(),
        });
        let received = async {
            navigate(&bidi, &context, react.url("index.html")).await?;
            let result: serde_json::Value = evaluate_json(
                &bidi,
                &context,
                &format!("import('./main.js').then(module => module.receiveNative({config}))"),
            )
            .await?;
            for field in ["video", "data", "wasm"] {
                assert_eq!(result[field], true, "installed Python to React: {field}");
            }
            let catalog_config = serde_json::json!({
                "endpoint": destination.endpoint(),
                "token": catalog_token.token,
                "publisherToken": metadata_token.token,
                "room": source_room.as_str(),
            });
            let catalog: bool = evaluate_json(
                &bidi,
                &context,
                &format!(
                    "import('./main.js').then(module => module.observeCatalog({catalog_config}))"
                ),
            )
            .await?;
            assert!(catalog, "real React catalog consumer and final state");
            navigate(&bidi, &context, generated_react.url("index.html")).await?;
            let decoded: bool = evaluate_json(
                &bidi,
                &context,
                &format!("import('./main.js').then(module => module.receiveGenerated({config}))"),
            )
            .await?;
            assert!(
                decoded,
                "packed generated Web handles decoded Native video in React"
            );
            navigate(&bidi, &context, fixture.url("tests/uniffi-fixture.html")).await?;
            let generated_contract = include_str!("contracts/uniffi-media-contract.js");
            let ffi: serde_json::Value = evaluate_json(
                &bidi,
                &context,
                &format!("((config) => {{ return {generated_contract} }})({config})"),
            )
            .await?;
            for field in [
                "initialState",
                "stableState",
                "pendingReadClosed",
                "joinedAndClosed",
                "exactVideoOwner",
                "trackIdentity",
                "streamIdentity",
            ] {
                assert_eq!(ffi[field], true, "generated browser ABI: {field}");
            }
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        }
        .await;
        use tokio::io::AsyncWriteExt as _;
        producer
            .stdin
            .take()
            .ok_or("missing producer shutdown pipe")?
            .write_all(b"stop\n")
            .await?;
        let stopped =
            tokio::time::timeout(std::time::Duration::from_secs(5), producer.wait()).await??;
        received?;
        assert!(
            stopped.success(),
            "installed Python producer failed: {stopped}"
        );
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
async fn server_restart_fences_old_agents() -> TestResult<()> {
    let destination = DestinationServer::start().await?;
    let address = destination.address();
    let server = StaticServer::start(root()).await?;
    let room = RoomId::new("web-adapter-restart")?;
    let expires_at = authorization_expiry()?;
    let sender = mint_development_token(&room, &ParticipantId::new("publisher")?, expires_at)?;
    let receiver = mint_development_token(&room, &ParticipantId::new("subscriber")?, expires_at)?;
    let start = include_str!("contracts/server-restart-start.js")
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
        let fenced: bool = evaluate_json(
            &bidi,
            &context,
            include_str!("contracts/server-restart-finish.js"),
        )
        .await?;
        assert!(fenced);
        let fresh: serde_json::Value = evaluate_json(&bidi, &context, &start).await?;
        assert_eq!(fresh["initial"], serde_json::json!([1]));
        let closed: bool = evaluate_json(
            &bidi,
            &context,
            "(async () => { const { sender, receiver, stream, pumping } = globalThis.__serverRestart; await stream.close(); await pumping; await Promise.all([sender.close(), receiver.close()]); delete globalThis.__serverRestart; return true; })()",
        ).await?;
        assert!(closed);
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })
    .await?;
    Ok(())
}
