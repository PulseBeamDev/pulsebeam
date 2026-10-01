"""Materialize ignored editor assets from Bazel, then install locked editor trees.

This is an explicit source-workspace refresh, not an application build. Rust
project discovery, generated-source/proc-macro metadata, diagnostics and
formatting are supplied by rules_rust's maintained rust-analyzer integration.
"""

import argparse
import os
from pathlib import Path
import shutil
import subprocess


def writable_directories(path):
    for directory, _, _ in os.walk(path):
        current = Path(directory)
        current.chmod(current.stat().st_mode | 0o700)


def copy(source, destination):
    destination.parent.mkdir(parents=True, exist_ok=True)
    if destination.is_symlink():
        destination.unlink()
    if source.is_dir():
        if destination.exists():
            writable_directories(destination)
            shutil.rmtree(destination)
        # Bazel artifacts are read-only; editor mirrors must be refreshable.
        shutil.copytree(source, destination, copy_function=shutil.copyfile)
        writable_directories(destination)
    else:
        if destination.exists():
            destination.chmod(destination.stat().st_mode | 0o600)
        shutil.copyfile(source, destination)


def main():
    parser = argparse.ArgumentParser()
    for name in ("pnpm", "web", "react", "bindings", "rust-lib", "web-module", "core-module",
                 "entrypoint", "next", "native"):
        parser.add_argument("--" + name, required=True)
    args = parser.parse_args()
    inputs = {key: Path(value).resolve() for key, value in vars(args).items()}
    root = Path(os.environ["BUILD_WORKSPACE_DIRECTORY"])
    web = root / "agents/pulsebeam-agent-web"
    copy(inputs["web"] / "dist", web / "dist")
    copy(inputs["react"] / "dist", root / "agents/react/dist")
    copy(inputs["bindings"], web / "generated/bindings")
    for key, name in (("rust_lib", "lib.rs"), ("web_module", "pulsebeam_agent_web_module.rs"),
                      ("core_module", "pulsebeam_agent_core_module.rs")):
        copy(inputs[key], web / "generated/wasm/src" / name)
    copy(inputs["entrypoint"], web / "generated/index.web.ts")
    copy(inputs["next"], root / "apps/meet/.next")
    copy(inputs["native"], root / "target/uniffi/native")
    for package in ("agents/pulsebeam-agent-web", "agents/react", "agents/react/tests/consumer", "apps/meet", "docs"):
        subprocess.run([str(inputs["pnpm"]), "install", "--frozen-lockfile", "--ignore-scripts"],
                       cwd=root / package, check=True,
                       env=os.environ | {"BAZEL_BINDIR": ".", "JS_BINARY__CHDIR": str(root / package),
                                         "JS_BINARY__PATCH_NODE_FS": "0"})
    print("Editor assets refreshed. Run ./bazel run //:rust_ide -- neovim for Rust LSP configuration.")


if __name__ == "__main__":
    main()
