#![allow(clippy::arithmetic_side_effects, clippy::expect_used)]

use std::fs;
use std::path::PathBuf;

#[test]
fn generated_package_has_strict_portable_boundaries() {
    let generated = PathBuf::from(
        std::env::var_os("PULSEBEAM_UNIFFI_BINDINGS")
            .expect("run //agents/pulsebeam-agent-web:uniffi_contract through ./bazel test"),
    );
    let core = fs::read_to_string(generated.join("pulsebeam_agent_core.ts"))
        .expect("core bindings must be generated");
    let web = fs::read_to_string(generated.join("pulsebeam_agent_web.ts"))
        .expect("web bindings must be generated");

    for boundary in [
        "AgentConfig",
        "DesiredState",
        "Snapshot",
        "Notification",
        "TopicMessage",
        "MediaFrame",
        "TransportStatistics",
        "AgentError",
    ] {
        assert!(
            core.contains(&format!("export type {boundary}")),
            "missing generated core boundary {boundary}"
        );
    }
    assert!(core.contains("payload: Uint8Array"));
    assert!(web.contains("export type WebMediaTrack = MediaStreamTrack"));
    assert!(web.contains("export type WebMediaStream = MediaStream"));
    assert!(!web.contains("export type WebMediaTrack = bigint"));
    assert!(!web.contains("export type WebMediaStream = bigint"));
    assert!(!web.contains("MediaRegistryProof"));
    assert!(!web.contains("normalizeAgentConfig"));
    for boundary in ["AgentConfig", "DesiredState", "Snapshot"] {
        let definition = record_definition(&core, boundary);
        assert!(
            !definition.contains("any"),
            "{boundary} contains an untyped policy/state field"
        );
    }
    let config = record_definition(&core, "AgentConfig");
    assert!(config.contains("token: string"));
    assert!(!config.contains("roomId"));
    assert!(!config.contains("requestHeaders"));
    assert!(!config.contains("manualSubscriptions"));

    // The strict consumer is compiled by the paired :uniffi_types Bazel test.
}

fn record_definition<'a>(source: &'a str, name: &str) -> &'a str {
    let marker = format!("export type {name} = {{");
    let start = source.find(&marker).expect("generated record exists");
    let remainder = source.get(start..).expect("record start is a boundary");
    let length = remainder
        .find("\n}\n")
        .expect("generated record has an end")
        + 2;
    remainder.get(..length).expect("record end is a boundary")
}
