#![allow(clippy::arithmetic_side_effects, clippy::expect_used)]

use std::fs;
use std::path::PathBuf;
use std::process::Command;

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
    assert!(web.contains("from \"./pulsebeam_agent_core\""));
    assert!(web.contains("export type WebMediaTrack = MediaStreamTrack"));
    assert!(web.contains("export type WebMediaStream = MediaStream"));
    assert!(!web.contains("export type WebMediaTrack = bigint"));
    assert!(!web.contains("export type WebMediaStream = bigint"));
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
    fs::write(
        &consumer,
        r#"import type {
  AgentConfig,
  AgentError,
  DesiredState,
  MediaFrame,
  Notification,
  Snapshot,
  TopicMessage,
  TransportStatistics,
  WebMediaStream,
  WebMediaTrack,
} from "../generated/index.web.js";
import { MediaRegistryProof, normalizeAgentConfig } from "../generated/index.web.js";

declare const config: AgentConfig;
declare const desired: DesiredState;
declare const snapshot: Snapshot;
declare const notification: Notification;
declare const topic: TopicMessage;
declare const frame: MediaFrame;
declare const statistics: TransportStatistics;
declare const error: AgentError;
declare const track: MediaStreamTrack;

const normalized: AgentConfig = normalizeAgentConfig(config);
const proof = new MediaRegistryProof();
const sameTrack: WebMediaTrack = proof.roundTripTrack(track);
const stream: WebMediaStream = proof.createStream();
const bytes: Uint8Array = frame.data;
void [desired, snapshot, notification, topic, statistics, error, normalized, sameTrack, stream, bytes];
"#,
    )
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
