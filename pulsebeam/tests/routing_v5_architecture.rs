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

#[test]
fn deleted_routing_v4_types_stay_deleted() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_owned();
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    for file in files {
        if file
            .components()
            .any(|component| component.as_os_str() == "tests")
        {
            continue;
        }
        let text = fs::read_to_string(&file).unwrap();
        for name in [
            "ClusterCommand",
            "AckGeneration",
            "left_right",
            "ShardEventWrapper",
        ] {
            assert!(
                !text.contains(name),
                "legacy routing type {name} in {file:?}"
            );
        }
    }
}
