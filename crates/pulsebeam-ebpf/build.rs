use sha2::{Digest, Sha256};
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

const BPF_LINKER_VERSION: &str = "0.11.0";
const BPF_LINKER_RELEASE: &str = "https://github.com/aya-rs/bpf-linker/releases/download";

struct ReleaseAsset {
    triple: &'static str,
    digest: &'static str,
    executable: &'static str,
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=BPF_LINKER");
    println!("cargo:rerun-if-env-changed=PATH");

    if env::var("CARGO_CFG_TARGET_ARCH").ok().as_deref() != Some("bpf") {
        return;
    }

    match resolve_linker() {
        Ok(linker) => {
            let parent = linker
                .parent()
                .ok_or_else(|| format!("linker has no parent directory: {}", linker.display()));
            let parent = match parent {
                Ok(parent) => parent,
                Err(error) => fail(error),
            };
            let mut paths = vec![parent.to_path_buf()];
            if let Some(path) = env::var_os("PATH") {
                paths.extend(env::split_paths(&path));
            }
            let path = match env::join_paths(paths) {
                Ok(path) => path,
                Err(error) => fail(format!("cannot construct linker PATH: {error}")),
            };
            println!("cargo:rustc-env=PATH={}", path.to_string_lossy());
        }
        Err(error) => fail(error),
    }
}

#[allow(
    clippy::exit,
    reason = "a build script must stop Cargo after reporting a resolver error"
)]
fn fail(error: String) -> ! {
    println!("cargo:warning=pulsebeam-ebpf: {error}");
    std::process::exit(1);
}

fn resolve_linker() -> Result<PathBuf, String> {
    if let Some(value) = env::var_os("BPF_LINKER") {
        return find_executable(&value).ok_or_else(|| {
            format!(
                "BPF_LINKER points to {:?}, but no executable was found",
                Path::new(&value)
            )
        });
    }

    if let Some(linker) = find_executable(OsStr::new("bpf-linker")) {
        return Ok(linker);
    }

    let asset = release_asset(env::var("HOST").map_err(|_| "HOST is not set".to_owned())?)?;
    let target_dir = cargo_target_dir()?;
    let cache = target_dir
        .join("bpf-linker")
        .join(BPF_LINKER_VERSION)
        .join(asset.triple);
    let executable = cache.join(asset.executable);
    if executable.is_file() {
        return Ok(executable);
    }

    fs::create_dir_all(&cache)
        .map_err(|error| format!("cannot create {}: {error}", cache.display()))?;
    let archive_name = format!("bpf-linker-{}.tar.zst", asset.triple);
    let archive = cache.join(&archive_name);
    if !archive.is_file() || !matches_digest(&archive, asset.digest)? {
        let temporary = cache.join(format!(".{archive_name}.{}.download", std::process::id()));
        let url = format!("{BPF_LINKER_RELEASE}/v{BPF_LINKER_VERSION}/{archive_name}");
        let status = Command::new("curl")
            .args([
                "--fail",
                "--location",
                "--silent",
                "--show-error",
                "--retry",
                "3",
                "--output",
            ])
            .arg(&temporary)
            .arg(&url)
            .status()
            .map_err(|error| format!("cannot run curl to fetch {url}: {error}"))?;
        if !status.success() {
            let _ = fs::remove_file(&temporary);
            return Err(format!("curl could not fetch {url}"));
        }
        if !matches_digest(&temporary, asset.digest)? {
            let _ = fs::remove_file(&temporary);
            return Err(format!(
                "downloaded {archive_name} failed its SHA-256 check"
            ));
        }
        fs::rename(&temporary, &archive)
            .or_else(|error| {
                if archive.is_file() {
                    Ok(())
                } else {
                    Err(error)
                }
            })
            .map_err(|error| format!("cannot cache {}: {error}", archive.display()))?;
    }

    let extraction = cache.join(format!(".extract-{}", std::process::id()));
    let _ = fs::remove_dir_all(&extraction);
    fs::create_dir(&extraction)
        .map_err(|error| format!("cannot create {}: {error}", extraction.display()))?;
    let status = Command::new("tar")
        .args(["--zstd", "--extract", "--file"])
        .arg(&archive)
        .args(["--directory"])
        .arg(&extraction)
        .status()
        .map_err(|error| format!("cannot run tar to unpack {}: {error}", archive.display()))?;
    if !status.success() {
        let _ = fs::remove_dir_all(&extraction);
        return Err(format!(
            "tar could not unpack {}; install tar with zstd support or set BPF_LINKER",
            archive.display()
        ));
    }

    let unpacked = find_named_file(&extraction, asset.executable)
        .ok_or_else(|| format!("{archive_name} did not contain {}", asset.executable))?;
    fs::rename(&unpacked, &executable)
        .or_else(|error| {
            if executable.is_file() {
                Ok(())
            } else {
                Err(error)
            }
        })
        .map_err(|error| format!("cannot cache {}: {error}", executable.display()))?;
    let _ = fs::remove_dir_all(&extraction);
    Ok(executable)
}

fn release_asset(host: String) -> Result<ReleaseAsset, String> {
    match host.as_str() {
        "x86_64-unknown-linux-gnu" | "x86_64-unknown-linux-musl" => Ok(ReleaseAsset {
            triple: "x86_64-unknown-linux-musl",
            digest: "10f62ba9ab7e544d538370552660efcb4f1a19153d5752bbf0f6b51f3bada450",
            executable: "bpf-linker",
        }),
        "aarch64-unknown-linux-gnu" | "aarch64-unknown-linux-musl" => Ok(ReleaseAsset {
            triple: "aarch64-unknown-linux-musl",
            digest: "d09ddd83303e9ab1443f51e0e284680154009646a3ce141c63d838ee61b73eb9",
            executable: "bpf-linker",
        }),
        "x86_64-apple-darwin" => Ok(ReleaseAsset {
            triple: "x86_64-apple-darwin",
            digest: "10eec9ff4397ec69d15e522ba6d579aecd8fc4cbec34d86cae7ea943bb5a9a55",
            executable: "bpf-linker",
        }),
        "aarch64-apple-darwin" => Ok(ReleaseAsset {
            triple: "aarch64-apple-darwin",
            digest: "d3b1952971472334e3f76760c33a6a97f151af45b9b89a70e06d92e5bb4a75",
            executable: "bpf-linker",
        }),
        other => Err(format!(
            "no pinned bpf-linker release exists for host {other}; set BPF_LINKER"
        )),
    }
}

fn cargo_target_dir() -> Result<PathBuf, String> {
    if let Some(target_dir) = env::var_os("CARGO_TARGET_DIR") {
        return Ok(PathBuf::from(target_dir));
    }
    let out_dir =
        PathBuf::from(env::var_os("OUT_DIR").ok_or_else(|| "OUT_DIR is not set".to_owned())?);
    out_dir
        .ancestors()
        .nth(4)
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("cannot derive target directory from {}", out_dir.display()))
}

fn find_executable(value: &OsStr) -> Option<PathBuf> {
    let value_path = Path::new(value);
    if value_path.components().count() > 1 {
        return value_path.is_file().then(|| value_path.to_path_buf());
    }
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|directory| directory.join(value_path))
        .find(|candidate| candidate.is_file())
}

fn find_named_file(root: &Path, name: &str) -> Option<PathBuf> {
    let entries = fs::read_dir(root).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.file_name().and_then(OsStr::to_str) == Some(name) && path.is_file() {
            return Some(path);
        }
        if path.is_dir()
            && let Some(found) = find_named_file(&path, name)
        {
            return Some(found);
        }
    }
    None
}

fn matches_digest(path: &Path, expected: &str) -> Result<bool, String> {
    let mut file =
        File::open(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("cannot hash {}: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        let bytes = buffer
            .get(..count)
            .ok_or_else(|| format!("read beyond checksum buffer for {}", path.display()))?;
        hasher.update(bytes);
    }
    let actual = format!("{:x}", hasher.finalize());
    Ok(actual == expected)
}
