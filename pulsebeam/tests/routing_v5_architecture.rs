use std::fs;
use std::path::{Path, PathBuf};

fn rust_files(root: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with('.') || name == "target")
            {
                continue;
            }
            rust_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

fn source(relative: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.parent()
        .map(|root| root.join(relative))
        .and_then(|path| fs::read_to_string(path).ok())
        .unwrap_or_default()
}

#[test]
fn routing_v5_architecture_guards() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_owned();
    let mut files = Vec::new();
    rust_files(&root, &mut files);

    for file in &files {
        if file
            .components()
            .any(|component| component.as_os_str() == "tests")
        {
            continue;
        }
        let text = fs::read_to_string(file).unwrap();
        assert!(
            !text.contains("ClusterCommand"),
            "legacy command in {file:?}"
        );
        assert!(
            !text.contains("AckGeneration"),
            "generation acknowledgement in {file:?}"
        );
        assert!(!text.contains("left_right"), "left-right state in {file:?}");
        assert!(
            !text.contains("ShardEventWrapper"),
            "wrapped shard event in {file:?}"
        );
        assert!(
            !text.contains("ParticipantControlEvent"),
            "legacy participant event in {file:?}"
        );
        assert!(
            !text.contains("LocalTrackKey"),
            "legacy track key in {file:?}"
        );
        assert!(!text.contains("DenseSlotMap"), "dense slot map in {file:?}");
    }

    let shard_root = root.join("pulsebeam/src/shard");
    let downstream_root = root.join("pulsebeam/src/participant/downstream");
    let mut hot_sources = Vec::new();
    rust_files(&shard_root, &mut hot_sources);
    rust_files(&downstream_root, &mut hot_sources);
    for file in &hot_sources {
        let text = fs::read_to_string(file).unwrap();
        assert!(!text.contains(".find("), "discovery scan in {file:?}");
        assert!(!text.contains(".find_map("), "discovery scan in {file:?}");
        assert!(!text.contains(".position("), "discovery scan in {file:?}");
        if file.starts_with(&shard_root) {
            assert!(!text.contains("oneshot"), "reply channel in {file:?}");
            assert!(!text.contains("SlotMap"), "stable-key arena in {file:?}");
        }
    }

    for file in rust_files_for(&downstream_root) {
        let text = fs::read_to_string(&file).unwrap();
        for line in text.lines() {
            if line.contains("HashMap<") {
                assert!(
                    !line.contains("TrackId")
                        && !line.contains("ParticipantId")
                        && !line.contains("Topic"),
                    "stable-id hash map in {file:?}: {line}"
                );
            }
        }
    }

    let router = source("pulsebeam/src/shard/router.rs");
    assert!(!router.is_empty(), "router source must be readable");
    assert!(
        !router.contains("to_vec()"),
        "router dispatch allocates packet copies"
    );
    assert!(
        !router.contains("to_string()"),
        "router dispatch allocates strings"
    );
    let worker = source("pulsebeam/src/shard/worker.rs");
    assert!(!worker.is_empty(), "worker source must be readable");
    assert!(!worker.contains("Sender<RecvPacketBatch>"));
    assert!(!worker.contains("Receiver<RecvPacketBatch>"));

    let routing = source("pulsebeam/src/shard/router.rs");
    let Some((_, routing_body)) = routing.split_once("pub(crate) trait RoutingContext") else {
        assert!(
            !routing.is_empty(),
            "routing context source must be readable"
        );
        return;
    };
    let body = routing_body
        .split_once("\n}\n")
        .map_or(routing_body, |(body, _)| body);
    for token in ["TrackId", "ParticipantId", "RoomId", "Topic"] {
        assert!(
            !body.contains(token),
            "stable identity in RoutingContext: {token}"
        );
    }
}

fn rust_files_for(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    rust_files(root, &mut files);
    files
}
