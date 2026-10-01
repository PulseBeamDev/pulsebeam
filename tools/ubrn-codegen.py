"""Generate UBRN's two SDK namespaces from Bazel-compiled native metadata.

UBRN needs Cargo workspace metadata for UniFFI configuration. Its narrowly
patched --no-deps queries use a declared offline snapshot, never a Cargo build.
Generated Rust is compiled and processed by wasm-bindgen in separate actions.
"""

import argparse
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import tomllib


def main():
    parser = argparse.ArgumentParser()
    for name in (
        "generator", "cargo", "rustc", "web-library", "core-library",
        "bindings-output", "lib-output", "web-module-output",
        "core-module-output", "entrypoint-output", "manifest-output",
    ):
        parser.add_argument("--" + name, required=True)
    args = parser.parse_args()
    generator = Path(args.generator).resolve()
    cargo = Path(args.cargo).resolve()
    rustc = Path(args.rustc).resolve()
    libraries = {
        "pulsebeam_agent_web": Path(args.web_library).resolve(),
        "pulsebeam_agent_core": Path(args.core_library).resolve(),
    }
    source = Path.cwd()
    manifest = tomllib.loads((source / "Cargo.toml").read_text())
    outputs = {
        "generated/wasm/src/lib.rs": Path(args.lib_output).resolve(),
        "generated/wasm/src/pulsebeam_agent_web_module.rs": Path(args.web_module_output).resolve(),
        "generated/wasm/src/pulsebeam_agent_core_module.rs": Path(args.core_module_output).resolve(),
        "generated/index.web.ts": Path(args.entrypoint_output).resolve(),
        "generated/wasm/Cargo.toml": Path(args.manifest_output).resolve(),
    }
    bindings = Path(args.bindings_output).resolve()
    with tempfile.TemporaryDirectory(prefix="pulsebeam-ubrn-") as temp:
        workspace = Path(temp) / "workspace"
        workspace.mkdir()
        for name in ("Cargo.toml", "Cargo.lock"):
            shutil.copyfile(source / name, workspace / name)
        for member in manifest["workspace"]["members"]:
            shutil.copytree(source / member, workspace / member)
        empty = Path("tools/rust/empty.rs")
        shutil.copyfile(source / empty, workspace / empty)
        home = Path(temp) / "cargo-home"
        home.mkdir()
        env = os.environ | {
            "CARGO": str(cargo), "RUSTC": str(rustc), "CARGO_HOME": str(home),
            "CARGO_NET_OFFLINE": "true", "RUSTC_WRAPPER": "",
            "RUSTC_WORKSPACE_WRAPPER": "",
        }
        package = workspace / "agents/pulsebeam-agent-web"
        for namespace, library in libraries.items():
            subprocess.run(
                [str(generator), "generate", "wasm", "bindings", "--library",
                 "--crate", namespace, "--no-format",
                 "--ts-dir", "generated/bindings", "--abi-dir", "generated/wasm/src",
                 str(library)],
                cwd=package, env=env, check=True,
            )
        subprocess.run(
            [str(generator), "generate", "wasm", "wasm-crate", "--config",
             "ubrn.config.yaml", *libraries],
            cwd=package, env=env, check=True,
        )
        generated = package / "generated/bindings"
        for namespace in libraries:
            if not (generated / f"{namespace}.ts").is_file():
                raise RuntimeError(f"UBRN did not emit {namespace}.ts")
        shutil.copytree(generated, bindings, dirs_exist_ok=True)
        for relative, output in outputs.items():
            output.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(package / relative, output)


if __name__ == "__main__":
    main()
