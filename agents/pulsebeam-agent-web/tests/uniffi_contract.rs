#![allow(clippy::arithmetic_side_effects, clippy::expect_used)]

use std::fs;
use std::path::PathBuf;
use std::process::Command;

const STRICT_CONSUMER: &str = include_str!("contracts/strict-consumer.ts");

#[test]
fn generated_package_has_strict_portable_boundaries() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let generated = root.join("generated/bindings");
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

    let consumer = root.join("target/uniffi-strict-consumer.ts");
    fs::create_dir_all(consumer.parent().expect("consumer has parent"))
        .expect("create consumer directory");
    fs::write(&consumer, STRICT_CONSUMER)
    .expect("write strict consumer");

    let status = Command::new("pnpm")
        .args([
            "exec",
            "tsc",
            "--strict",
            "--noEmit",
            "--target",
            "ES2022",
            "--module",
            "ESNext",
            "--moduleResolution",
            "Bundler",
            "--lib",
            "ES2022,DOM,DOM.Iterable",
            "--allowJs",
        ])
        .arg(root.join("web/assets.d.ts"))
        .arg(&consumer)
        .current_dir(&root)
        .status()
        .expect("run TypeScript compiler");
    assert!(status.success(), "strict generated-package consumer failed");
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
