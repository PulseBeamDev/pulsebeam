"""Exercise workspace formatting with the provisioned tools and their runfiles."""
import argparse
import importlib.util
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("workspace_fix", Path(__file__).resolve().parents[1] / "fix.py")
fixer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixer)


class WorkspaceFixTests(unittest.TestCase):
    def test_formats_workspace_outside_runfiles(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "Cargo.toml").write_text(
                '[workspace]\nmembers = ["crates/sample"]\n'
                '[workspace.package]\nedition = "2024"\n')
            rust = root / "crates/sample/src/main.rs"
            rust.parent.mkdir(parents=True)
            rust.write_text("fn main( ) { }\n")
            javascript = []
            for member in ("agents/pulsebeam-agent-web", "agents/react", "apps/meet"):
                source = root / member / "sample.ts"
                source.parent.mkdir(parents=True)
                source.write_text("export const value=1\n")
                javascript.append(source)
            arguments = ["fix", "--rustfmt", RUSTFMT]
            for name in ("web", "react", "meet"):
                arguments.extend(["--" + name, FORMATTER])
            # A py_binary launched by bazel run does not supply test runfiles variables.
            env = {key: value for key, value in os.environ.items()
                   if key not in {"TEST_SRCDIR", "RUNFILES", "RUNFILES_DIR", "RUNFILES_MANIFEST_FILE"}}
            env["BUILD_WORKSPACE_DIRECTORY"] = str(root)
            with patch.dict(os.environ, env, clear=True), patch.object(sys, "argv", arguments), \
                    patch.object(fixer, "apply_clippy"):
                fixer.main()
            self.assertEqual(rust.read_text(), "fn main() {}\n")
            for source in javascript:
                self.assertEqual(source.read_text(), "export const value = 1;\n")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--rustfmt", required=True)
    parser.add_argument("--formatter", required=True)
    args, rest = parser.parse_known_args()
    RUSTFMT = args.rustfmt
    FORMATTER = args.formatter
    unittest.main(argv=[sys.argv[0], *rest])
