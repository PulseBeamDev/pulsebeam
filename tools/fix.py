"""Explicitly format tracked Rust and JavaScript sources with provisioned tools."""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
from urllib.parse import unquote, urlparse
import tomllib


def clippy_diagnostics(root: Path) -> list[Path]:
    """Collect only outputs of this Bazel invocation, never stale bazel-bin files."""
    with tempfile.TemporaryDirectory(prefix="pulsebeam-clippy-") as directory:
        bep = Path(directory) / "events.json"
        subprocess.run([
            str(root / "bazel"), "build",
            "--aspects=@rules_rust//rust:defs.bzl%rust_clippy_aspect",
            "--output_groups=clippy_output",
            "--@rules_rust//rust/settings:clippy_output_diagnostics",
            f"--build_event_json_file={bep}",
            "//...",
        ], cwd=root, check=True)
        sets = {}
        roots = []
        for line in bep.read_text().splitlines():
            event = json.loads(line)
            identifier = event.get("id", {})
            if "namedSet" in identifier:
                sets[identifier["namedSet"]["id"]] = event.get("namedSetOfFiles", {})
            for completed in (event.get("completed", {}), event.get("aspectResult", {})):
                for group in completed.get("outputGroup", []):
                    if group.get("name") == "clippy_output":
                        roots.extend(item["id"] for item in group.get("fileSets", []))
        seen, paths = set(), set()

        def visit(key):
            if key in seen:
                return
            seen.add(key)
            item = sets.get(key, {})
            for file in item.get("files", []):
                uri = file.get("uri", "")
                parsed = urlparse(uri)
                if parsed.scheme == "file" and parsed.path.endswith(".clippy.diagnostics"):
                    paths.add(Path(unquote(parsed.path)))
            for child in item.get("fileSets", []):
                visit(child["id"])

        for key in roots:
            visit(key)
        # The caller must consume the output before Bazel starts another build.
        # Return the output content snapshot rather than relying on future Bazel state.
        return [(path, path.read_bytes()) for path in sorted(paths)]


def apply_clippy(root: Path):
    module_path = Path(__file__).with_name("clippy-fix.py")
    spec = importlib.util.spec_from_file_location("pulsebeam_clippy_fix", module_path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    output = clippy_diagnostics(root)
    with tempfile.TemporaryDirectory(prefix="pulsebeam-clippy-results-") as directory:
        diagnostics = []
        for index, (_, data) in enumerate(output):
            path = Path(directory) / f"{index}.json"
            path.write_bytes(data)
            diagnostics.append(path)
        print(f"Applied {module.apply(root, diagnostics)} machine-applicable Clippy edits.", flush=True)


def main():
    parser = argparse.ArgumentParser()
    for name in ("rustfmt", "web", "react", "meet"):
        parser.add_argument("--" + name, required=True)
    args = parser.parse_args()
    # Keep launcher runfiles symlinks intact when changing the subprocess cwd.
    tools = {key: str(Path(value).absolute()) for key, value in vars(args).items()}
    root = Path(os.environ["BUILD_WORKSPACE_DIRECTORY"])
    apply_clippy(root)
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
        if tool == "react":
            files.extend(str(path) for path in (root / "docs/react").glob("*.tsx"))
        subprocess.run([tools[tool], "--write", *sorted(files)], cwd=package,
                       env=os.environ | {"BAZEL_BINDIR": ".", "JS_BINARY__CHDIR": str(package),
                                         "JS_BINARY__PATCH_NODE_FS": "0"}, check=True)


if __name__ == "__main__":
    main()
