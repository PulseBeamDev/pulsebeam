mod support;

use pulsebeam_core::{
    auth::mint_development_token,
    identity::{ParticipantExternalId, RoomExternalId},
};
use serde::Deserialize;
use std::error::Error;
use std::path::PathBuf;
use support::{DestinationServer, StaticServer, TestResult, capabilities, evaluate_json, navigate};
use thirtyfour::prelude::WebDriver;
use thirtyfour::testing::run_browser_test;

const PUBLIC: &str = include_str!("contracts/observe-public.js");
const START_PUBLIC: &str = include_str!("contracts/start-public.js");
const LIVE: &str = include_str!("contracts/live-agent-contract.js");
const UNIFFI: &str = include_str!("contracts/uniffi-media-contract.js");
const LOAD: &str = include_str!("contracts/load-web.js");
const FAILURE: &str = include_str!("contracts/initialization-failure.js");
const REJECTIONS: &str = include_str!("contracts/unhandled-rejections.js");
const RUNTIME_LOCAL_OPERATIONS: &str =
    include_str!("contracts/runtime-local-operation-contract.js");
const REMOTE_MEDIA: &str = include_str!("contracts/remote-media-contract.js");
const REMOTE_CATALOG: &str = include_str!("contracts/remote-catalog-contract.js");
const TOPIC_FACADE: &str = include_str!("contracts/topic-facade-contract.js");
const REACT: &str = include_str!("../../react/tests/browser/observe.js");

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
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Live {
    connected: bool,
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
    external_identity: bool,
    inert_policy: bool,
    invalid_atomic: bool,
    max_physical_height: bool,
    capacity: bool,
    hidden_floor: bool,
    mapping_not_removal: bool,
    removed_terminal: bool,
    audio_explicit: bool,
    playback_scoped: bool,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TopicFacade {
    stable: bool,
    inert: bool,
    subscribed: bool,
    received: bool,
    published: bool,
    aborted: bool,
    released: bool,
    coalesced: bool,
    malformed_fenced: bool,
    validation: bool,
    post_close: bool,
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
    audio_explicit: bool,
}
fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

async fn web(server: &StaticServer, failure: bool) -> TestResult<()> {
    let url = server.url("tests/fixture.html");
    run_browser_test(WebDriver::managed(capabilities()?), |driver| async move {
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
                    "attachRemoteAudio",
                    "attachRemoteMedia",
                    "attachRemoteVideo",
                    "createAgent",
                    "createCaptureSource",
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
    run_browser_test(WebDriver::managed(capabilities()?), |driver| async move {
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
    run_browser_test(WebDriver::managed(capabilities()?), |driver| async move {
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
    run_browser_test(WebDriver::managed(capabilities()?), |driver| async move {
        let bidi = driver.bidi().await?;
        let context = bidi.browsing_context().top_level().await?;
        navigate(&bidi, &context, url).await?;
        let result: RemoteCatalog = evaluate_json(&bidi, &context, REMOTE_CATALOG).await?;
        assert!(
            result.external_identity
                && result.inert_policy
                && result.invalid_atomic
                && result.max_physical_height
                && result.capacity
                && result.hidden_floor
                && result.mapping_not_removal
                && result.removed_terminal
                && result.audio_explicit
                && result.playback_scoped,
            "remote catalog contract: {result:?}",
        );
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    })
    .await
    .map_err(|error| format!("remote catalog contract failed: {error}").into())
}

#[tokio::test(flavor = "multi_thread")]
async fn typed_topic_facade_tracks_lifetime_and_bounds_delivery() -> TestResult<()> {
    let server = StaticServer::start(root()).await?;
    let url = server.url("tests/fixture.html");
    run_browser_test(WebDriver::managed(capabilities()?), |driver| async move {
        let bidi = driver.bidi().await?;
        let context = bidi.browsing_context().top_level().await?;
        navigate(&bidi, &context, url).await?;
        let result: TopicFacade = evaluate_json(&bidi, &context, TOPIC_FACADE).await?;
        assert!(
            result.stable
                && result.inert
                && result.subscribed
                && result.received
                && result.published
                && result.aborted
                && result.released
                && result.coalesced
                && result.malformed_fenced
                && result.validation
                && result.post_close,
            "typed topic contract: {result:?}",
        );
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    })
    .await
    .map_err(|error| format!("typed topic contract failed: {error}").into())
}

#[tokio::test(flavor = "multi_thread")]
async fn initialization_failure_is_private_and_deterministic() -> TestResult<()> {
    let server = StaticServer::start_with_wasm_failure(root(), true).await?;
    web(&server, true).await?;
    assert_eq!(server.wasm_requests(), 1);
    Ok(())
}
#[tokio::test(flavor = "multi_thread")]
async fn public_agent_connects_and_delivers_remote_media() -> TestResult<()> {
    let _destination = DestinationServer::start()?;
    let server = StaticServer::start(root()).await?;
    let url = server.url("tests/fixture.html");
    let room = RoomExternalId::new("public-web-contract")?;
    let sender =
        mint_development_token(&room, &ParticipantExternalId::new("web-sender")?, u64::MAX)?;
    let receiver = mint_development_token(
        &room,
        &ParticipantExternalId::new("web-receiver")?,
        u64::MAX,
    )?;
    let live = LIVE
        .replace("__SENDER_TOKEN__", &sender)
        .replace("__RECEIVER_TOKEN__", &receiver);
    run_browser_test(WebDriver::managed(capabilities()?), |driver| async move {
        let bidi = driver.bidi().await?;
        let context = bidi.browsing_context().top_level().await?;
        navigate(&bidi, &context, url).await?;
        let _: () = evaluate_json(&bidi, &context, LOAD).await?;
        let r: Live = evaluate_json(&bidi, &context, &live).await?;
        assert!(
            r.connected
                && r.discovered
                && r.delivered
                && r.reconnected
                && r.default_recreated
                && r.topic_metadata
                && r.runtime_failure_event
                && r.close_during_local_operation
                && r.caller_owns_track
        );
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    })
    .await
    .map_err(|e| format!("live browser contract failed: {e}").into())
}
#[tokio::test(flavor = "multi_thread")]
async fn generated_media_types_run_through_bidi() -> TestResult<()> {
    let server = StaticServer::start(root()).await?;
    let url = server.url("tests/uniffi-fixture.html");
    run_browser_test(WebDriver::managed(capabilities()?), |driver| async move {
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
#[tokio::test(flavor = "multi_thread")]
async fn react_provider_contract_runs_through_bidi() -> TestResult<()> {
    let fixture = root().join("../react/tests/browser/dist");
    let manifest = fixture.join("fixture-manifest.json");
    if !fixture.join("index.html").is_file() || !manifest.is_file() {
        return Err(
            "React browser fixture is missing; run `just --justfile agents/react/Justfile browser-fixture`"
                .into(),
        );
    }
    let manifest_time = std::fs::metadata(&manifest)?.modified()?;
    for source in [
        root().join("../react/tests/browser/fixture.tsx"),
        root().join("../react/tests/browser/acquisition.tsx"),
        root().join("../react/tests/browser/ownership.tsx"),
        root().join("../react/tests/browser/playback.tsx"),
        root().join("../react/tests/browser/index.html"),
        root().join("dist/index.js"),
    ] {
        if std::fs::metadata(source)?.modified()? > manifest_time {
            return Err(
                "React browser fixture is stale; run `just --justfile agents/react/Justfile browser-fixture`"
                    .into(),
            );
        }
    }
    let server = StaticServer::start(fixture).await?;
    let url = server.url("index.html");
    run_browser_test(WebDriver::managed(capabilities()?), |driver| async move {
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
                && r.audio_explicit,
            "React browser contract result: {r:?}"
        );
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    })
    .await
    .map_err(|e| format!("React browser contract failed: {e}").into())
}
