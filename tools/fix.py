"""Explicitly format tracked Rust and JavaScript sources with provisioned tools."""

import argparse
import os
from pathlib import Path
import subprocess
import tomllib


def main():
    parser = argparse.ArgumentParser()
    for name in ("rustfmt", "web", "react", "meet"):
        parser.add_argument("--" + name, required=True)
    args = parser.parse_args()
    tools = {key: str(Path(value).resolve()) for key, value in vars(args).items()}
    root = Path(os.environ["BUILD_WORKSPACE_DIRECTORY"])
    workspace = tomllib.loads((root / "Cargo.toml").read_text())
    sources = []
    for member in workspace["workspace"]["members"]:
        package = root / member
        for folder in ("src", "tests", "benches", "examples"):
            sources.extend(str(path) for path in (package / folder).rglob("*.rs")
                           if not path.read_text().startswith("// This file is @generated"))
        if (package / "build.rs").exists():
            sources.append(str(package / "build.rs"))
    subprocess.run([tools["rustfmt"], "--edition", workspace["workspace"]["package"]["edition"], *sorted(sources)],
                   cwd=root, check=True)
    for tool, member in (("web", "agents/pulsebeam-agent-web"), ("react", "agents/react"), ("meet", "apps/meet")):
        package = root / member
        files = []
        for directory, folders, names in os.walk(package):
            folders[:] = [name for name in folders if name not in {"node_modules", "dist", "generated", "target", ".next"}]
            for name in names:
                path = Path(directory) / name
                if path.suffix in {".ts", ".tsx", ".js", ".mjs", ".json", ".css"} or name in {"ubrn.config.yaml", "pnpm-workspace.yaml"}:
                    files.append(str(path))
        subprocess.run([tools[tool], "--write", *sorted(files)], cwd=package,
                       env=os.environ | {"BAZEL_BINDIR": "."}, check=True)


if __name__ == "__main__":
    main()
