"""Derive Bazel tool tags from the repository's existing authoritative pins.

MODULE tags cannot read Cargo.lock or rust-toolchain.toml. This is pin
translation, not dependency resolution; crate-universe resolves the crates.
"""

import argparse
import difflib
import json
import os
from pathlib import Path
import re
import subprocess
import tomllib
from urllib.parse import parse_qs, urlsplit
from urllib.request import urlopen


def rust_archive_hashes(version):
    with urlopen(f"https://static.rust-lang.org/dist/channel-rust-{version}.toml") as response:
        manifest = tomllib.loads(response.read().decode())
    hosts = ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"]
    components = ["cargo", "clippy-preview", "llvm-tools-preview", "rust-analyzer-preview",
                  "rust-src", "rust-std", "rustc", "rustfmt-preview"]
    hashes = {}
    for component in components:
        targets = ["*"] if component == "rust-src" else hosts
        if component == "rust-std":
            targets = hosts + ["wasm32-unknown-unknown"]
        for target in targets:
            artifact = manifest["pkg"][component]["target"][target]
            if not artifact["available"]:
                raise ValueError(f"Rust {version} lacks {component} for {target}")
            archive = Path(urlsplit(artifact["xz_url"]).path).name
            digest = artifact["xz_hash"]
            if not re.fullmatch(r"[0-9a-f]{64}", digest):
                raise ValueError(f"invalid Rust archive checksum: {archive}")
            hashes[archive] = digest
    return dict(sorted(hashes.items()))


def locked_package(lock, name, spec):
    selectors = {key: [spec[key]] for key in ("branch", "rev", "tag") if key in spec}
    matches = []
    for package in lock:
        source = urlsplit(package.get("source", "").removeprefix("git+"))
        if (package["name"] == name and source._replace(query="", fragment="").geturl() == spec["git"]
                and parse_qs(source.query) == selectors):
            matches.append(package)
    if len(matches) != 1:
        raise ValueError(f"expected one locked {name} package for {spec}, found {len(matches)}")
    return matches[0]


def str0m_visibility_patch(source):
    """Expose only RTC's required internals, without modifying Cargo's checkout."""
    source = Path(source)
    files = {source / "src/lib.rs", source / "src/dtls.rs"}
    for module in ("crypto", "change", "format", "sdp", "rtp"):
        folder = source / "src" / module
        children = set(folder.rglob("*.rs"))
        if not folder.is_dir() or not children:
            raise ValueError(f"str0m visibility: missing Rust source tree {folder}")
        files.update(children)
    for required in files:
        if not required.is_file():
            raise ValueError(f"str0m visibility: missing required file {required}")
    result = []
    for path in sorted(files):
        before = path.read_bytes()
        after = before
        if path == source / "src/lib.rs":
            for module in ("crypto", "dtls", "change", "format", "sdp", "rtp_"):
                declaration = rb"^([ \t]*)(pub(?:\(crate\))?[ \t]+)?(mod[ \t]+" + module.encode() + rb"[ \t]*;)"
                matches = list(re.finditer(declaration, after, re.MULTILINE))
                if len(matches) != 1:
                    raise ValueError(f"str0m visibility: expected one root declaration for {module} in {path}, found {len(matches)}")
                after = re.sub(declaration, lambda m: m[1] + (m[2].replace(b"pub(crate)", b"pub") if m[2] else b"pub ") + m[3],
                               after, flags=re.MULTILINE)
        else:
            after = re.sub(rb"^([ \t]*)pub\(crate\)", rb"\1pub", after, flags=re.MULTILINE)
            after = re.sub(rb"^([ \t]*)mod(?=[ \t]+[A-Za-z_])", rb"\1pub mod", after, flags=re.MULTILINE)
        relative = path.relative_to(source).as_posix().encode()
        if before != after:
            result.append(b"diff --git a/" + relative + b" b/" + relative + b"\n")
        for line in difflib.diff_bytes(difflib.unified_diff, before.splitlines(keepends=True),
                                      after.splitlines(keepends=True), b"a/" + relative, b"b/" + relative):
            result.append(line if line.endswith(b"\n") else line + b"\n\\ No newline at end of file\n")
    return b"".join(result)


def rtc_visibility(root, lock, deps, metadata):
    packages = {name: locked_package(lock, name, deps[name]) for name in ("str0m", "is")}
    if packages["str0m"]["source"] != packages["is"]["source"]:
        raise ValueError("RTC str0m and is must resolve from the same Git source and revision")
    selected = packages["str0m"]
    matches = [p for p in metadata["packages"]
               if (p["name"], p["version"], p.get("source")) == (selected["name"], selected["version"], selected["source"])]
    if len(matches) != 1:
        raise ValueError(f"expected one resolved RTC str0m manifest for {selected}, found {len(matches)}")
    patch = str0m_visibility_patch(Path(matches[0]["manifest_path"]).parent)
    (root / "tools/patches/str0m-rtc-visibility.patch").write_bytes(patch)
    return selected, bool(patch)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--cargo", required=True)
    parser.add_argument("--rustc", required=True)
    args = parser.parse_args()
    root = Path(os.environ.get("BUILD_WORKSPACE_DIRECTORY", Path(__file__).resolve().parents[1]))
    rust = tomllib.loads((root / "rust-toolchain.toml").read_text())["toolchain"]["channel"]
    lock = tomllib.loads((root / "Cargo.lock").read_text())["package"]
    wasm = next(p["version"] for p in lock if p["name"] == "wasm-bindgen")
    ubrn = tomllib.loads((root / "tools/generators.toml").read_text())["ubrn"]
    workspace = tomllib.loads((root / "Cargo.toml").read_text())
    rtc = tomllib.loads((root / "crates/pulsebeam-rtc/Cargo.toml").read_text())
    native = tomllib.loads((root / "agents/pulsebeam-agent-native/Cargo.toml").read_text())
    uniffi = native["dependencies"]["uniffi"]["version"]
    members = workspace["workspace"]["members"]
    web_config = (root / "agents/pulsebeam-agent-web/ubrn.config.yaml").read_text()
    runtime = re.search(r"^\s+runtimeVersion:\s*(\S+)\s*$", web_config, re.MULTILINE)
    if runtime is None:
        raise ValueError("ubrn.config.yaml must pin web.runtimeVersion")
    proof = root / "tools/rust/wasm_proof"
    proof.mkdir(parents=True, exist_ok=True)
    (proof / "Cargo.toml").write_text(
        '# Generated by //tools:update-rust-pins. Do not edit.\n'
        '[package]\nname = "pulsebeam-wasm-proof-deps"\nversion = "0.0.0"\nedition = "2024"\n'
        '[lib]\npath = "../empty.rs"\n[dependencies]\n'
        'pulsebeam-agent-web = { path = "../../../agents/pulsebeam-agent-web", features = ["uniffi"] }\n'
        'pulsebeam-agent-core = { path = "../../../agents/pulsebeam-agent-core" }\n'
        f'uniffi-runtime-javascript = {{ version = "{runtime[1]}", features = ["wasm32"] }}\n'
        f'wasm-bindgen = "={wasm}"\n'
    )
    wasm_members = ["agents/pulsebeam-agent-web", "agents/pulsebeam-agent-core", "crates/pulsebeam-proto"]
    wasm_specs = {}
    wasm_deps = {}
    for member in wasm_members:
        owner = tomllib.loads((root / member / "Cargo.toml").read_text())
        wasm_deps[member] = []
        for alias, declared in owner.get("dependencies", {}).items():
            spec = {"version": declared} if isinstance(declared, str) else declared.copy()
            if spec.pop("workspace", False):
                inherited = workspace["workspace"]["dependencies"][alias]
                inherited = {"version": inherited} if isinstance(inherited, str) else inherited.copy()
                features = inherited.get("features", []) + spec.get("features", [])
                spec = inherited | spec
                if features:
                    spec["features"] = features
            if "path" in spec:
                continue
            package = spec.get("package", alias)
            combined = wasm_specs.setdefault(package, {"version": spec["version"], "default_features": False, "features": set()})
            combined["default_features"] |= spec.get("default-features", True)
            combined["features"].update(spec.get("features", []))
            wasm_deps[member].append(f"@wasm_crates//:{package}")
    wasm_specs["uniffi-runtime-javascript"] = {"version": runtime[1], "default_features": True, "features": {"wasm32"}}
    (root / "tools/wasm-deps.bzl").write_text(
        "# Generated from normal owner dependencies by //tools:update-rust-pins. Do not edit.\n"
        'load("@crates//:defs.bzl", native_deps = "all_crate_deps")\n\n'
        + "WASM_DEPS = " + json.dumps(wasm_deps, indent=4) + "\n\n"
        + '''def all_crate_deps(**kwargs):
    if kwargs == {"normal": True}:
        return select({
            "@platforms//cpu:wasm32": WASM_DEPS[native.package_name()],
            "//conditions:default": native_deps(**kwargs),
        })
    return native_deps(**kwargs)
'''

    )
    metadata_inputs = [f"//{member}:cargo_metadata_inputs" for member in members]
    (root / "tools/rust-metadata.bzl").write_text(
        "# Generated by //tools:update-rust-pins. Do not edit.\n"
        + "CARGO_METADATA_INPUTS = " + json.dumps(metadata_inputs, indent=4) + "\n"
    )
    # crate-universe keys packages by (name, version), discarding Cargo source IDs.
    # Isolate each str0m source with stock rules, preserving root lock revisions.
    sources = {
        "str0m_native": {"str0m": workspace["workspace"]["dependencies"]["str0m"]},
        "str0m_rtc": {name: rtc["dependencies"][name] for name in ("str0m", "is")},
    }
    # A separate Cargo workspace avoids native-only feature unification while
    # retaining every resolved package identity from the authoritative root lock.
    sources["wasm_crates"] = {
        package: {"version": spec["version"], "default-features": spec["default_features"],
                  "features": sorted(spec["features"])}
        for package, spec in wasm_specs.items()
    }
    for name, deps in sources.items():
        folder = root / "tools/rust" / name
        folder.mkdir(parents=True, exist_ok=True)
        manifest = '# Generated by //tools:update-rust-pins. Do not edit.\n[workspace]\n[package]\n'
        manifest += f'name = "{name}"\nversion = "0.0.0"\nedition = "2024"\n[lib]\npath = "../empty.rs"\n[dependencies]\n'
        for dep, spec in deps.items():
            fields = ", ".join(f"{key} = {json.dumps(value)}" for key, value in spec.items())
            manifest += f"{dep} = {{ {fields} }}\n"
        (folder / "Cargo.toml").write_text(manifest)
        (folder / "Cargo.lock").write_bytes((root / "Cargo.lock").read_bytes())
        env = os.environ | {"RUSTC": str(Path(args.rustc).resolve()), "RUSTC_WRAPPER": "", "RUSTC_WORKSPACE_WRAPPER": ""}
        resolved = subprocess.run([str(Path(args.cargo).resolve()), "metadata", "--format-version=1", "--manifest-path", str(folder / "Cargo.toml")],
                                  env=env, check=True, stdout=subprocess.PIPE, text=True)
        derived = tomllib.loads((folder / "Cargo.lock").read_text())["package"]
        identities = {(p["name"], p["version"], p.get("source")) for p in lock}
        unexpected = [p for p in derived if p["name"] != name and (p["name"], p["version"], p.get("source")) not in identities]
        if unexpected:
            raise ValueError(f"{name} resolved dependencies outside the authoritative root lock: {unexpected}")
        if name == "str0m_rtc":
            rtc_package, has_visibility_patch = rtc_visibility(root, lock, deps, json.loads(resolved.stdout))
    hashes = json.dumps(rust_archive_hashes(rust), indent=4)
    fragment = f'''# Generated by //tools:update-rust-pins. Do not edit.
RUST_ARCHIVE_SHA256S = {hashes}

host = use_extension("@rules_rust//rust:extensions.bzl", "rust_host_tools")
host.host_tools(name = "pulsebeam_rust_host_tools", version = "{rust}", sha256s = RUST_ARCHIVE_SHA256S)
use_repo(host, "pulsebeam_rust_host_tools")

rust = use_extension("@rules_rust//rust:extensions.bzl", "rust")
rust.toolchain(
    edition = "2024",
    versions = ["{rust}"],
    rust_analyzer_version = "{rust}",
    rustfmt_version = "{rust}",
    sha256s = RUST_ARCHIVE_SHA256S,
)
use_repo(rust, "rust_toolchains")
register_toolchains("@rust_toolchains//:all")

crate = use_extension("@rules_rust//crate_universe:extensions.bzl", "crate")
crate.spec(package = "uniffi", version = "{uniffi}", default_features = False, features = ["cli", "cargo-metadata"], repositories = ["build_tools"])
crate.spec(package = "wasm-bindgen-cli", version = "={wasm}", repositories = ["build_tools"])
crate.annotation(crate = "wasm-bindgen-cli", gen_binaries = ["wasm-bindgen"], repositories = ["build_tools"])
crate.spec(
    package = "uniffi-bindgen-react-native",
    git = "{ubrn['git']}",
    rev = "{ubrn['rev']}",
    repositories = ["build_tools"],
)
crate.annotation(crate = "uniffi-bindgen-react-native", gen_binaries = ["uniffi-bindgen-react-native"], repositories = ["build_tools"])
# Library-mode metadata needs workspace namespace configs, not dependency resolution.
crate.annotation(crate = "ubrn_common", patches = ["//tools:patches/ubrn-offline-workspace-metadata.patch"], patch_args = ["-p1"], repositories = ["build_tools"])
crate.from_specs(
    name = "build_tools",
    supported_platform_triples = ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"],
    host_tools = "@pulsebeam_rust_host_tools//:BUILD.bazel",
)
use_repo(crate, "build_tools")
'''
    for name in sources:
        triples = ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"]
        if name == "wasm_crates":
            triples.append("wasm32-unknown-unknown")
        fragment += f'''
crate.from_cargo(
    name = "{name}",
    manifests = ["//tools:rust/{name}/Cargo.toml"],
    cargo_lockfile = "//tools:rust/{name}/Cargo.lock",
    skip_cargo_lockfile_overwrite = True,
    supported_platform_triples = {json.dumps(triples)},
    host_tools = "@pulsebeam_rust_host_tools//:BUILD.bazel",
)
use_repo(crate, "{name}")
'''
    for name in ("str0m_native", "str0m_rtc"):
        spec = sources[name]["str0m"]
        package = locked_package(lock, "str0m", spec)
        fragment += f'crate.annotation(crate = "str0m", version = "={package["version"]}", override_target_lib = "@{name}//:str0m", repositories = ["crates"])\n'
    fragment += 'crate.annotation(crate = "is", override_target_lib = "@str0m_rtc//:is", repositories = ["crates"])\n'
    if has_visibility_patch:
        fragment += f'crate.annotation(crate = "str0m", version = "={rtc_package["version"]}", patches = ["//tools:patches/str0m-rtc-visibility.patch"], patch_args = ["-p1"], repositories = ["str0m_rtc"])\n'
    (root / "tools/rust-pins.MODULE.bazel").write_text(fragment)


if __name__ == "__main__":
    main()
