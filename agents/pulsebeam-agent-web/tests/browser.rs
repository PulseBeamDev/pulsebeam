#[path = "support/destination.rs"]
mod destination;
mod support;
use destination::DestinationServer;

use pulsebeam_core::{
    auth::{DEVELOPMENT_API_KEY_ID, DEVELOPMENT_API_SIGNING_KEY, DEVELOPMENT_PROJECT_ID},
    identity::{ParticipantExternalId, RoomExternalId},
};
use serde::Deserialize;
use std::error::Error;
use std::path::PathBuf;
use support::{StaticServer, TestResult, capabilities, evaluate_json, managed_driver, navigate};
use thirtyfour::prelude::ChromiumLikeCapabilities;
use thirtyfour::testing::run_browser_test;

const PUBLIC: &str = include_str!("contracts/observe-public.js");
const START_PUBLIC: &str = include_str!("contracts/start-public.js");
const LIVE: &str = include_str!("contracts/live-agent-contract.js");
const RELIABLE_RESTART_START: &str = include_str!("contracts/reliable-restart-start.js");
const RELIABLE_RESTART_FINISH: &str = include_str!("contracts/reliable-restart-finish.js");
const UNIFFI: &str = include_str!("contracts/uniffi-media-contract.js");
const LOAD: &str = include_str!("contracts/load-web.js");
const FAILURE: &str = include_str!("contracts/initialization-failure.js");
const REJECTIONS: &str = include_str!("contracts/unhandled-rejections.js");
const RUNTIME_LOCAL_OPERATIONS: &str =
    include_str!("contracts/runtime-local-operation-contract.js");
const REMOTE_MEDIA: &str = include_str!("contracts/remote-media-contract.js");
const REMOTE_CATALOG: &str = include_str!("contracts/remote-catalog-contract.js");

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Public {
    exports: Vec<String>,
    independent: bool,
    config_copied: bool,
    initial_stable: bool,
    initial_frozen: bool,
    initial: String,
    latest_only: bool,
    close_before_settlement: bool,
    local_operations: bool,
    local_handles: bool,
    validation_rejected: bool,
    scoped_logging: bool,
    serialization_failure_nonterminal: bool,
    failure_event: bool,
    core_validation_event: bool,
    caller_owns_track: bool,
    no_removed_listener_calls: bool,
    closed: String,
    post_close: bool,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Live {
    fixed_rtp: RtpEvidence,
    default_rtp: RtpEvidence,
    independent_rendering: bool,
    mounted_removal: bool,
    connected: bool,
    external_identity: bool,
    discovered: bool,
    delivered: bool,
    reconnected: bool,
    default_recreated: bool,
    topic_metadata: bool,
    runtime_failure_event: bool,
    close_during_local_operation: bool,
    caller_owns_track: bool,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReliableStart {
    initial: Vec<u8>,
    generation: u64,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReliableFinish {
    events: Vec<ReliableEvent>,
    late_events: Vec<Vec<u8>>,
    no_historical_replay: bool,
    rejoined: bool,
}
#[derive(Debug, Deserialize)]
struct ReliableEvent {
    sequence: u64,
    payload: Vec<u8>,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RtpEvidence {
    relay: usize,
    ssrc: u32,
    packets: u64,
    with_extension: u64,
    first_value: Option<Vec<u8>>,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeLocalOperations {
    serialized_before_release: bool,
    final_track_wins: bool,
    close_fenced: bool,
    post_close_fenced: bool,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteMedia {
    first_available: bool,
    unchanged: bool,
    combined: bool,
    replaced: bool,
    removed: bool,
    restored: bool,
    isolated: bool,
    terminal: bool,
    reported: bool,
    retried: bool,
    pending_suppressed: bool,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteCatalog {
    max_physical_height: bool,
    measured_visibility: bool,
    released: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UniFfi {
    track_identity: bool,
    stream_identity: bool,
    foreign_rejected: bool,
    stale_rejected: bool,
    exhaustion_rejected: bool,
    retained_before_track_release: String,
    retained_after_track_release: String,
    retained_before_stream_release: String,
    retained_after_stream_release: String,
}
fn root() -> PathBuf {
    PathBuf::from(
        std::env::var_os("PULSEBEAM_WEB_FIXTURE_ROOT")
            .expect("run SDK browser contracts through ./bazel test"),
    )
}

async fn web(server: &StaticServer, failure: bool) -> TestResult<()> {
    let url = server.url("tests/fixture.html");
    run_browser_test(managed_driver(capabilities()?), |driver| async move {
        let bidi = driver.bidi().await?;
        let context = bidi.browsing_context().top_level().await?;
        navigate(&bidi, &context, url).await?;
        let _: () = evaluate_json(&bidi, &context, LOAD).await?;
        if failure {
            let persisted: bool = evaluate_json(&bidi, &context, FAILURE).await?;
            let rejections: usize = evaluate_json(&bidi, &context, REJECTIONS).await?;
            assert!(persisted);
            assert_eq!(rejections, 0);
        } else {
            let _: () = evaluate_json(&bidi, &context, START_PUBLIC).await?;
            let result: Public = evaluate_json(&bidi, &context, PUBLIC).await?;
            assert_eq!(
                result.exports,
                [
                    "LocalTrackCapacityError",
                    "attachRemoteMedia",
                    "attachRemoteVideo",
                    "createAgent",
                    "createCaptureSource",
                    "createManagedAgent",
                    "nativeCaptureTrack"
                ]
            );
            assert!(
                result.independent
                    && result.config_copied
                    && result.initial_stable
                    && result.initial_frozen
                    && result.latest_only
                    && result.close_before_settlement
                    && result.local_operations
                    && result.local_handles
                    && result.validation_rejected
                    && result.scoped_logging
                    && result.serialization_failure_nonterminal
                    && result.failure_event
                    && result.core_validation_event
                    && result.caller_owns_track
                    && result.no_removed_listener_calls
                    && result.post_close,
                "public contract result: {result:?}",
            );
            assert_eq!(result.initial, "disconnected");
            assert_eq!(result.closed, "disconnected");
        }
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    })
    .await
    .map_err(|e| format!("browser contract failed: {e}").into())
}

#[tokio::test(flavor = "multi_thread")]
async fn public_agent_contract_runs_through_bidi() -> TestResult<()> {
    let server = StaticServer::start(root()).await?;
    web(&server, false).await?;
    assert_eq!(server.wasm_requests(), 1);
    Ok(())
}
#[tokio::test(flavor = "multi_thread")]
async fn runtime_local_operations_are_serialized_and_close_fenced() -> TestResult<()> {
    let server = StaticServer::start(root()).await?;
    let url = server.url("tests/fixture.html");
    run_browser_test(managed_driver(capabilities()?), |driver| async move {
        let bidi = driver.bidi().await?;
        let context = bidi.browsing_context().top_level().await?;
        navigate(&bidi, &context, url).await?;
        let result: RuntimeLocalOperations =
            evaluate_json(&bidi, &context, RUNTIME_LOCAL_OPERATIONS).await?;
        assert!(
            result.serialized_before_release
                && result.final_track_wins
                && result.close_fenced
                && result.post_close_fenced,
            "runtime local operation result: {result:?}",
        );
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    })
    .await
    .map_err(|error| format!("runtime local operation contract failed: {error}").into())
}
#[tokio::test(flavor = "multi_thread")]
async fn remote_media_attachment_is_stable_and_terminal() -> TestResult<()> {
    let server = StaticServer::start(root()).await?;
    let url = server.url("tests/fixture.html");
    run_browser_test(managed_driver(capabilities()?), |driver| async move {
        let bidi = driver.bidi().await?;
        let context = bidi.browsing_context().top_level().await?;
        navigate(&bidi, &context, url).await?;
        let _: () = evaluate_json(&bidi, &context, LOAD).await?;
        let result: RemoteMedia = evaluate_json(&bidi, &context, REMOTE_MEDIA).await?;
        assert!(
            result.first_available
                && result.unchanged
                && result.combined
                && result.replaced
                && result.removed
                && result.restored
                && result.isolated
                && result.terminal
                && result.reported
                && result.retried
                && result.pending_suppressed,
            "remote media contract result: {result:?}",
        );
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    })
    .await
    .map_err(|error| format!("remote media contract failed: {error}").into())
}
#[tokio::test(flavor = "multi_thread")]
async fn remote_catalog_handles_preserve_identity_and_bound_demand() -> TestResult<()> {
    let server = StaticServer::start(root()).await?;
    let url = server.url("tests/fixture.html");
    run_browser_test(managed_driver(capabilities()?), |driver| async move {
        let bidi = driver.bidi().await?;
        let context = bidi.browsing_context().top_level().await?;
        navigate(&bidi, &context, url).await?;
        let result: RemoteCatalog = evaluate_json(&bidi, &context, REMOTE_CATALOG).await?;
        assert!(
            result.max_physical_height && result.measured_visibility && result.released,
            "remote catalog contract: {result:?}",
        );
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    })
    .await
    .map_err(|error| format!("remote catalog contract failed: {error}").into())
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_audio_routes_real_decoded_receivers_without_ui() -> TestResult<()> {
    let server = StaticServer::start(root()).await?;
    let mut browser = capabilities()?;
    browser.add_arg("--autoplay-policy=no-user-gesture-required")?;
    let url = server.url("tests/fixture.html");
    run_browser_test(managed_driver(browser), |driver| async move {
        let bidi = driver.bidi().await?;
        let context = bidi.browsing_context().top_level().await?;
        navigate(&bidi, &context, url).await?;
        let result: bool = evaluate_json(
            &bidi,
            &context,
            include_str!("contracts/remote-audio-contract.js"),
        )
        .await?;
        assert!(result);
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    })
    .await
    .map_err(|error| format!("decoded remote audio contract failed: {error}").into())
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_audio_recovers_from_a_trusted_gesture_and_fences_close() -> TestResult<()> {
    let server = StaticServer::start(root()).await?;
    let mut browser = capabilities()?;
    browser.add_arg("--autoplay-policy=document-user-activation-required")?;
    let url = server.url("tests/fixture.html");
    run_browser_test(managed_driver(browser), |driver| async move {
        let bidi = driver.bidi().await?;
        let context = bidi.browsing_context().top_level().await?;
        navigate(&bidi, &context, url).await?;
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
        let result: bool = evaluate_json(
            &bidi,
            &context,
            include_str!("contracts/remote-audio-recovery-finish.js"),
        )
        .await?;
        assert!(result);
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    })
    .await
    .map_err(|error| format!("remote audio recovery contract failed: {error}").into())
}

#[tokio::test(flavor = "multi_thread")]
async fn initialization_failure_is_private_and_deterministic() -> TestResult<()> {
    let server = StaticServer::start_with_wasm_failure(root(), true).await?;
    web(&server, true).await?;
    assert_eq!(server.wasm_requests(), 1);
    Ok(())
}
#[tokio::test(flavor = "multi_thread")]
async fn connected_agents_select_and_play_audio_without_ui() -> TestResult<()> {
    let destination = DestinationServer::start().await?;
    let server = StaticServer::start(root()).await?;
    let room = RoomExternalId::new("automatic-web-audio")?;
    let sender = pulsebeam_server::sign_participant_token(
        &DEVELOPMENT_PROJECT_ID.as_str(),
        &DEVELOPMENT_API_KEY_ID.as_str(),
        &DEVELOPMENT_API_SIGNING_KEY.to_secret_string(),
        room.as_str(),
        ParticipantExternalId::new("web-sender")?.as_str(),
        u64::MAX,
    )?;
    let receiver = pulsebeam_server::sign_participant_token(
        &DEVELOPMENT_PROJECT_ID.as_str(),
        &DEVELOPMENT_API_KEY_ID.as_str(),
        &DEVELOPMENT_API_SIGNING_KEY.to_secret_string(),
        room.as_str(),
        ParticipantExternalId::new("web-receiver")?.as_str(),
        u64::MAX,
    )?;
    let pinned = pulsebeam_server::sign_participant_token(
        &DEVELOPMENT_PROJECT_ID.as_str(),
        &DEVELOPMENT_API_KEY_ID.as_str(),
        &DEVELOPMENT_API_SIGNING_KEY.to_secret_string(),
        room.as_str(),
        ParticipantExternalId::new("web-pinned")?.as_str(),
        u64::MAX,
    )?;
    let expression = include_str!("contracts/automatic-audio-contract.js")
        .replace("__DESTINATION_ENDPOINT__", &destination.endpoint())
        .replace("__SENDER_TOKEN__", &sender)
        .replace("__RECEIVER_TOKEN__", &receiver)
        .replace("__SECOND_RECEIVER_TOKEN__", &pinned);
    let url = server.url("tests/fixture.html");
    let mut browser = capabilities()?;
    browser.add_arg("--autoplay-policy=no-user-gesture-required")?;
    run_browser_test(managed_driver(browser), |driver| async move {
        let bidi = driver.bidi().await?;
        let context = bidi.browsing_context().top_level().await?;
        navigate(&bidi, &context, url).await?;
        let _: () = evaluate_json(&bidi, &context, LOAD).await?;
        let result: bool = evaluate_json(&bidi, &context, &expression).await?;
        assert!(result);
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    })
    .await
    .map_err(|error| format!("automatic Agent audio contract failed: {error}").into())
}

#[tokio::test(flavor = "multi_thread")]
async fn public_agent_connects_and_delivers_remote_media() -> TestResult<()> {
    let destination = DestinationServer::start().await?;
    let server = StaticServer::start(root()).await?;
    let url = server.url("tests/fixture.html");
    let room = RoomExternalId::new("public-web-contract")?;
    let sender = pulsebeam_server::sign_participant_token(
        &DEVELOPMENT_PROJECT_ID.as_str(),
        &DEVELOPMENT_API_KEY_ID.as_str(),
        &DEVELOPMENT_API_SIGNING_KEY.to_secret_string(),
        (room).as_str(),
        (ParticipantExternalId::new("web-sender")?).as_str(),
        u64::MAX,
    )?;
    let receiver = pulsebeam_server::sign_participant_token(
        &DEVELOPMENT_PROJECT_ID.as_str(),
        &DEVELOPMENT_API_KEY_ID.as_str(),
        &DEVELOPMENT_API_SIGNING_KEY.to_secret_string(),
        (room).as_str(),
        (ParticipantExternalId::new("web-receiver")?).as_str(),
        u64::MAX,
    )?;
    let second_receiver = pulsebeam_server::sign_participant_token(
        &DEVELOPMENT_PROJECT_ID.as_str(),
        &DEVELOPMENT_API_KEY_ID.as_str(),
        &DEVELOPMENT_API_SIGNING_KEY.to_secret_string(),
        (room).as_str(),
        (ParticipantExternalId::new("web-second-receiver")?).as_str(),
        u64::MAX,
    )?;
    let live = LIVE
        .replace("__DESTINATION_ENDPOINT__", &destination.endpoint())
        .replace("__SENDER_TOKEN__", &sender)
        .replace("__RECEIVER_TOKEN__", &receiver)
        .replace("__SECOND_RECEIVER_TOKEN__", &second_receiver);
    run_browser_test(managed_driver(capabilities()?), |driver| async move {
        let bidi = driver.bidi().await?;
        let context = bidi.browsing_context().top_level().await?;
        navigate(&bidi, &context, url).await?;
        let _: () = evaluate_json(&bidi, &context, LOAD).await?;
        let r: Live = evaluate_json(&bidi, &context, &live).await?;
        assert_ne!(r.fixed_rtp.relay, r.default_rtp.relay, "{r:?}");
        assert!(
            r.fixed_rtp.packets >= 4
                && r.fixed_rtp.with_extension > 0
                && r.fixed_rtp.first_value.as_deref() == Some(&[0, 0xa0, 10]),
            "fixed-delay positive control SSRC {}: {:?}",
            r.fixed_rtp.ssrc,
            r.fixed_rtp,
        );
        assert!(
            r.default_rtp.packets >= 4
                && r.default_rtp.with_extension == 0
                && r.default_rtp.first_value.is_none(),
            "fresh-default receiver SSRC {}: {:?}",
            r.default_rtp.ssrc,
            r.default_rtp,
        );
        assert!(
            r.independent_rendering
                && r.mounted_removal
                && r.connected
                && r.external_identity
                && r.discovered
                && r.delivered
                && r.reconnected
                && r.default_recreated
                && r.topic_metadata
                && r.runtime_failure_event
                && r.close_during_local_operation
                && r.caller_owns_track,
            "{r:?}"
        );
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    })
    .await
    .map_err(|e| format!("live browser contract failed: {e}").into())
}
#[tokio::test(flavor = "multi_thread")]
async fn reliable_topics_recover_the_lost_tail_through_server_restart() -> TestResult<()> {
    let destination = DestinationServer::start().await?;
    let destination_address = destination.address();
    let endpoint = destination.endpoint();
    let server = StaticServer::start(root()).await?;
    let url = server.url("tests/fixture.html");
    let room = RoomExternalId::new("reliable-restart-contract")?;
    let sender = pulsebeam_server::sign_participant_token(
        &DEVELOPMENT_PROJECT_ID.as_str(),
        &DEVELOPMENT_API_KEY_ID.as_str(),
        &DEVELOPMENT_API_SIGNING_KEY.to_secret_string(),
        (room).as_str(),
        (ParticipantExternalId::new("publisher")?).as_str(),
        u64::MAX,
    )?;
    let receiver = pulsebeam_server::sign_participant_token(
        &DEVELOPMENT_PROJECT_ID.as_str(),
        &DEVELOPMENT_API_KEY_ID.as_str(),
        &DEVELOPMENT_API_SIGNING_KEY.to_secret_string(),
        (room).as_str(),
        (ParticipantExternalId::new("subscriber")?).as_str(),
        u64::MAX,
    )?;
    let late = pulsebeam_server::sign_participant_token(
        &DEVELOPMENT_PROJECT_ID.as_str(),
        &DEVELOPMENT_API_KEY_ID.as_str(),
        &DEVELOPMENT_API_SIGNING_KEY.to_secret_string(),
        (room).as_str(),
        (ParticipantExternalId::new("late")?).as_str(),
        u64::MAX,
    )?;
    let start = RELIABLE_RESTART_START
        .replace("__DESTINATION_ENDPOINT__", &endpoint)
        .replace("__SENDER_TOKEN__", &sender)
        .replace("__RECEIVER_TOKEN__", &receiver);
    let finish = RELIABLE_RESTART_FINISH
        .replace("__DESTINATION_ENDPOINT__", &endpoint)
        .replace("__LATE_TOKEN__", &late);
    run_browser_test(managed_driver(capabilities()?), |driver| async move {
        let bidi = driver.bidi().await?;
        let context = bidi.browsing_context().top_level().await?;
        navigate(&bidi, &context, url).await?;
        let _: () = evaluate_json(&bidi, &context, LOAD).await?;
        let before: ReliableStart = evaluate_json(&bidi, &context, &start).await?;
        assert_eq!(before.initial, [1]);
        assert!(before.generation > 0);
        drop(destination);
        let _: () = evaluate_json(&bidi, &context,
            "(async () => { globalThis.__reliableRestart.sender.sendTopic('chat', 'ordered', new Uint8Array([2])); return null; })()"
        ).await?;
        let _restarted = DestinationServer::bind(destination_address).await?;
        let after: ReliableFinish = evaluate_json(&bidi, &context, &finish).await?;
        assert!(after.rejoined);
        assert!(after.no_historical_replay);
        assert_eq!(after.late_events, [[4]]);
        assert_eq!(after.events.iter().map(|event| (&event.sequence, &event.payload)).collect::<Vec<_>>(),
            vec![(&0, &vec![1]), (&1, &vec![2]), (&2, &vec![3]), (&3, &vec![4])]);
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    })
    .await
    .map_err(|e| format!("reliable restart browser contract failed: {e}").into())
}

#[tokio::test(flavor = "multi_thread")]
async fn generated_media_types_run_through_bidi() -> TestResult<()> {
    let server = StaticServer::start(root()).await?;
    let url = server.url("tests/uniffi-fixture.html");
    run_browser_test(managed_driver(capabilities()?), |driver| async move {
        let bidi = driver.bidi().await?;
        let context = bidi.browsing_context().top_level().await?;
        navigate(&bidi, &context, url).await?;
        let r: UniFfi = evaluate_json(&bidi, &context, UNIFFI).await?;
        assert!(
            r.track_identity
                && r.stream_identity
                && r.foreign_rejected
                && r.stale_rejected
                && r.exhaustion_rejected
        );
        assert_eq!(r.retained_before_track_release, "1");
        assert_eq!(r.retained_after_track_release, "0");
        assert_eq!(r.retained_before_stream_release, "1");
        assert_eq!(r.retained_after_stream_release, "0");
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    })
    .await
    .map_err(|e| format!("UniFFI browser contract failed: {e}").into())
}
