"""Check declared JS sources after dereferencing Bazel test-runfiles symlinks.

Prettier rejects explicit symlink inputs and silently ignores globbed ones.
The writable snapshot contains only declared inputs, never source-workspace files.
"""

import argparse
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--formatter", required=True)
    parser.add_argument("--source", required=True)
    parser.add_argument("patterns", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    patterns = args.patterns[1:] if args.patterns[:1] == ["--"] else args.patterns
    if not patterns:
        parser.error("at least one source pattern is required")
    formatter = str(Path(args.formatter).absolute())
    with tempfile.TemporaryDirectory(prefix="pulsebeam-prettier-") as temp:
        snapshot = Path(temp) / "sources"
        shutil.copytree(Path(args.source), snapshot, symlinks=False)
        env = os.environ | {
            "JS_BINARY__CHDIR": str(snapshot),
            "JS_BINARY__PATCH_NODE_FS": "0",
        }
        return subprocess.run([formatter, "--check", *patterns], env=env).returncode


if __name__ == "__main__":
    raise SystemExit(main())
