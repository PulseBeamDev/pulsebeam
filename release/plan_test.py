"""Release tags select one package version, never every matching workspace version."""

import argparse
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib
import unittest


class ReleasePlanTest(unittest.TestCase):
    def run_plan(self, manifest, *args):
        return subprocess.run(
            [sys.executable, str(Path(__file__).with_name("global.py")),
             "--manifest", str(manifest), "--config", INPUTS.config,
             "--template", INPUTS.template, *args],
            text=True, capture_output=True,
        )

    def test_canonical_tag_matches_authoritative_version(self):
        package = tomllib.loads(Path(INPUTS.manifest).read_text())["package"]
        distribution = tomllib.loads(Path(INPUTS.config).read_text())["distribution"]
        tag = distribution["tag-prefix"] + package["version"]
        result = self.run_plan(INPUTS.manifest, "--tag", tag)
        self.assertEqual(result.returncode, 0, result.stderr)
        plan = json.loads(result.stdout)
        self.assertEqual(plan["tag"], tag)
        self.assertEqual(plan["version"], package["version"])
        self.assertEqual(plan["targets"], distribution["targets"])

    def test_tag_rejection_and_installer_identity(self):
        distribution = tomllib.loads(Path(INPUTS.config).read_text())["distribution"]
        prefix = distribution["tag-prefix"]
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest = root / "Cargo.toml"
            cases = (("1.2.3", False), ("1.2.3-rc.1", True),
                     ("1.2.3+build-1", False), ("1.2.3-rc.1+build-1", True))
            for version, prerelease in cases:
                manifest.write_text(f'[package]\nname = "pulsebeam"\nversion = "{version}"\n')
                tag = prefix + version
                for invalid in (version, "v" + version, "pulsebeam/" + version,
                                prefix + "9.9.9", "other-package-v" + version):
                    with self.subTest(version=version, tag=invalid):
                        output = root / "rejected"
                        result = self.run_plan(manifest, "--tag", invalid, "--out", str(output))
                        self.assertNotEqual(result.returncode, 0)
                        self.assertIn("does not match authoritative package version", result.stderr)
                        self.assertFalse(output.exists(), "invalid tags must not assemble artifacts")
                output = root / version
                result = self.run_plan(manifest, "--tag", tag, "--out", str(output))
                self.assertEqual(result.returncode, 0, result.stderr)
                plan = json.loads((output / "release-plan.json").read_text())
                self.assertEqual(plan["tag"], tag)
                self.assertEqual(plan["prerelease"], prerelease)
                installer = (output / "pulsebeam-installer.sh").read_text()
                self.assertIn(f"TAG='{tag}'", installer)
                self.assertIn(f"VERSION='{version}'", installer)
                self.assertIn('releases/download/$TAG', installer)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    for name in ("manifest", "config", "template"):
        parser.add_argument("--" + name, required=True)
    INPUTS = parser.parse_args()
    unittest.main(argv=[sys.argv[0]])
