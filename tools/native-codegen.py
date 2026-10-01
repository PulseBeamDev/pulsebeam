"""Run UniFFI's CLI with a declared, isolated Cargo metadata workspace.

Library-mode UniFFI still queries Cargo for namespace configuration. Cargo is
used only for --metadata-no-deps, never as a second compilation pipeline.
"""

import argparse
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import tomllib


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--generator", required=True)
    parser.add_argument("--cargo", required=True)
    parser.add_argument("--rustc", required=True)
    parser.add_argument("--library", required=True)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    generator = Path(args.generator).resolve()
    cargo = Path(args.cargo).resolve()
    rustc = Path(args.rustc).resolve()
    library = Path(args.library).resolve()
    output = Path(args.output).resolve()
    source = Path.cwd()
    manifest = tomllib.loads((source / "Cargo.toml").read_text())
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="pulsebeam-uniffi-") as temp:
        workspace = Path(temp) / "workspace"
        workspace.mkdir()
        for name in ("Cargo.toml", "Cargo.lock"):
            shutil.copyfile(source / name, workspace / name)
        for member in manifest["workspace"]["members"]:
            shutil.copytree(source / member, workspace / member)
        shutil.copyfile(source / "tools/rust/empty.rs", workspace / "tools/rust/empty.rs")
        home = Path(temp) / "cargo-home"
        home.mkdir()
        env = os.environ | {
            "CARGO": str(cargo),
            "RUSTC": str(rustc),
            "CARGO_HOME": str(home),
            "CARGO_NET_OFFLINE": "true",
            "RUSTC_WRAPPER": "",
            "RUSTC_WORKSPACE_WRAPPER": "",
        }
        subprocess.run(
            [str(generator), "generate", "--library", "--metadata-no-deps",
             "--language", "kotlin", "--language", "swift", "--language", "python",
             "--no-format", "--out-dir", str(output), str(library)],
            cwd=workspace, env=env, check=True,
        )
    core = output / "uniffi/pulsebeam_agent_core/pulsebeam_agent_core.kt"
    native = output / "dev/pulsebeam/agent/pulsebeam_agent_native.kt"
    if not core.is_file() or not native.is_file():
        raise RuntimeError("UniFFI did not emit both the core and native Kotlin namespaces")
    text = native.read_text()
    if "import uniffi.pulsebeam_agent_core.AgentConfig" not in text:
        raise RuntimeError("native Kotlin bindings do not import the shared core AgentConfig")
    if re.search(r"^data class (AgentConfig|DesiredState|MediaFrame|Snapshot)\b", text, re.MULTILINE):
        raise RuntimeError("native Kotlin bindings duplicated a shared core DTO")
    for name in ("PulseBeamAgent.swift", "pulsebeam_agent_native.py", "pulsebeam_agent_core.py"):
        if not (output / name).is_file():
            raise RuntimeError(f"UniFFI did not emit {name}")


if __name__ == "__main__":
    main()
