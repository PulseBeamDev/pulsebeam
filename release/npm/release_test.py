"""Exercise release identity, packed artifacts, and publication boundaries."""

import argparse
import copy
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

from release.npm import release

parser = argparse.ArgumentParser()
parser.add_argument("--manifest", action="append", type=Path, required=True)
parser.add_argument("--archive", action="append", type=Path, required=True)
parser.add_argument("--npm", type=Path, required=True)
args, unittest_args = parser.parse_known_args()
MANIFESTS = {document["name"]: document for document in (
    json.loads(path.read_text()) for path in args.manifest
)}


class ReleaseContract(unittest.TestCase):
    def test_bazel_provisioned_npm_cli(self):
        self.assertEqual(subprocess.check_output([
            str(args.npm.resolve()), "--version",
        ], text=True).strip(), "11.6.4")

    def test_package_scoped_versions(self):
        for key, name in release.PACKAGES.items():
            version = MANIFESTS[name]["version"]
            self.assertEqual(release.plan(f"npm-{key}-v{version}", MANIFESTS), {
                "name": name, "version": version,
                "dist_tag": "next" if "-" in version else "latest",
            })
        for tag in ("v0.1.0", "pulsebeam-v0.1.0", "npm-react-v9.0.0", "npm-unknown-v0.1.0"):
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                release.plan(tag, MANIFESTS)

    def test_prereleases_never_replace_latest(self):
        manifests = copy.deepcopy(MANIFESTS)
        manifests["@pulsebeam/react"]["version"] = "0.2.0-rc.1"
        self.assertEqual(release.plan("npm-react-v0.2.0-rc.1", manifests)["dist_tag"], "next")
        for version in ("01.2.0", "0.2.0-rc.01", "0.2.0+build", "0.2.0-", "0.2.0/other"):
            manifests["@pulsebeam/react"]["version"] = version
            with self.subTest(version=version), self.assertRaises(ValueError):
                release.plan("npm-react-v" + version, manifests)

    def test_actual_public_archives(self):
        names = set()
        for path in args.archive:
            with tarfile.open(path) as archive:
                name = json.load(archive.extractfile("package/package.json"))["name"]
            release.validate_archive(path, MANIFESTS[name])
            names.add(name)
        self.assertEqual(names, set(release.PACKAGES.values()))

    def test_rejects_missing_export(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "bad.tgz"
            with tarfile.open(path, "w:gz") as archive:
                for name, body in {
                    "package/package.json": json.dumps(MANIFESTS["@pulsebeam/server"]).encode(),
                    "package/README.md": b"readme", "package/LICENSE": b"license",
                }.items():
                    member = tarfile.TarInfo(name)
                    member.size = len(body)
                    archive.addfile(member, io.BytesIO(body))
            with self.assertRaisesRegex(ValueError, "public export"):
                release.validate_archive(path, MANIFESTS["@pulsebeam/server"])

    def test_publication_requires_oidc(self):
        with patch.dict(os.environ, {}, clear=True), patch("subprocess.run") as command:
            with self.assertRaisesRegex(ValueError, "OIDC"):
                release.publish({}, MANIFESTS["@pulsebeam/server"], Path("package.tgz"), "npm")
            command.assert_not_called()

    def test_publish_uses_validated_archive_and_explicit_channel(self):
        environment = {
            "GITHUB_ACTIONS": "true", "ACTIONS_ID_TOKEN_REQUEST_URL": "fixture",
            "ACTIONS_ID_TOKEN_REQUEST_TOKEN": "fixture",
        }
        manifest = MANIFESTS["@pulsebeam/react"]
        with patch.dict(os.environ, environment, clear=True), patch("subprocess.run") as command, patch(
            "subprocess.check_output", return_value='"0.1.0"'
        ) as view:
            release.publish({"dist_tag": "next"}, manifest, Path("validated.tgz"), "bazel-npm")
            self.assertIn("@pulsebeam/web@" + manifest["dependencies"]["@pulsebeam/web"], view.call_args.args[0])
            command.assert_called_once_with([
                "bazel-npm", "publish", "validated.tgz", "--access", "public",
                "--tag", "next", "--provenance", "--ignore-scripts",
                "--registry", release.REGISTRY,
            ], check=True)
            view.return_value = ""
            command.reset_mock()
            with self.assertRaisesRegex(ValueError, "compatible"):
                release.publish({"dist_tag": "latest"}, manifest, Path("validated.tgz"), "bazel-npm")
            command.assert_not_called()


if __name__ == "__main__":
    unittest.main(argv=[__file__] + unittest_args)
