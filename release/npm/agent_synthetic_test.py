"""Agent release checks independent of the native Bazel package target."""

import copy
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest

from release.npm import release


MANIFEST = {
    "name": "@pulsebeam/agent",
    "version": "1.2.3-rc.1",
    "type": "module",
    "os": ["linux"],
    "cpu": ["x64"],
    "libc": ["glibc"],
    "engines": {"node": ">=22 <23"},
    "publishConfig": {"access": "public"},
    "dependencies": {"@ubjs/core": "0.31.0-5", "@ubjs/node": "0.31.0-5"},
    "exports": {
        ".": {"types": "./dist/index.d.ts", "import": "./dist/index.js"},
        "./core": {"types": "./dist/core.d.ts", "import": "./dist/core.js"},
        "./native": {"types": "./dist/native.d.ts", "import": "./dist/native.js"},
    },
}
DOCUMENTS = (
    "README.md", "LICENSE", "THIRD_PARTY_NOTICES.md", "RUST_STANDARD_LIBRARY_NOTICES.html",
    "NODE_ADDON_SOURCE_MANIFEST.json", "NODE_ADDON_SOURCE_NOTICES.json",
    "NODE_ADDON_SOURCE_PROVENANCE.txt", "NODE_ADDON_RUST_STANDARD_LIBRARY_NOTICES.html",
)


def contents(manifest):
    files = {"package/" + name: b"content" for name in DOCUMENTS}
    files["package/dist/libpulsebeam_agent_native.so"] = b"\x7fELFfixture"
    for conditions in manifest["exports"].values():
        for entry in conditions.values():
            files["package/" + entry[2:]] = b"generated"
    return files


def validate(manifest, files, expected=None):
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / "package.tgz"
        with tarfile.open(path, "w:gz") as archive:
            for name, body in {**files, "package/package.json": json.dumps(manifest).encode()}.items():
                member = tarfile.TarInfo(name)
                member.size = len(body)
                archive.addfile(member, io.BytesIO(body))
        return release.validate_archive(path, manifest if expected is None else expected)


class AgentSyntheticTest(unittest.TestCase):
    def test_plan(self):
        manifests = {MANIFEST["name"]: copy.deepcopy(MANIFEST)}
        self.assertEqual(release.plan("npm-agent-v1.2.3-rc.1", manifests), {
            "name": "@pulsebeam/agent", "version": "1.2.3-rc.1", "dist_tag": "next",
        })
        manifests[MANIFEST["name"]]["version"] = "1.2.3"
        self.assertEqual(release.package_plan("agent", manifests)["dist_tag"], "latest")
        for tag in ("npm-agent-v1.2.4", "npm-agent-v1.2.3-rc.1", "npm-agent-1.2.3", "v1.2.3"):
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                release.plan(tag, manifests)
        for version in ("01.2.3", "1.2.3-rc.01", "1.2.3+build", "1.2.3-", "1.2.3/x"):
            manifests[MANIFEST["name"]]["version"] = version
            with self.subTest(version=version), self.assertRaises(ValueError):
                release.plan("npm-agent-v" + version, manifests)

    def test_valid_archive_and_all_declared_exports(self):
        manifest = copy.deepcopy(MANIFEST)
        manifest["exports"]["./extra"] = {"types": "./dist/extra.d.ts", "import": "./dist/extra.js"}
        files = contents(manifest)
        self.assertEqual(validate(manifest, files), manifest)
        for conditions in manifest["exports"].values():
            for entry in conditions.values():
                broken = files.copy()
                del broken["package/" + entry[2:]]
                with self.subTest(entry=entry), self.assertRaises(ValueError):
                    validate(manifest, broken)

    def test_metadata_and_dependencies_fail_closed(self):
        cases = [
            ("type", "commonjs"), ("os", ["linux", "darwin"]), ("cpu", ["arm64"]),
            ("libc", ["musl"]), ("engines", {"node": ">=22"}),
            ("dependencies", {"@ubjs/core": "0.31.0-5", "@ubjs/node": "^0.31.0-5"}),
            ("version", "1.2.3-rc.01"), ("private", True),
            ("optionalDependencies", {"local": "file:../local"}),
            ("devDependencies", {"local": "workspace:*"}),
            ("peerDependencies", {"local": "link:../local"}),
            ("dependencies", {"@ubjs/core": "0.31.0-5", "@ubjs/node": "0.31.0-5", "local": "../local"}),
        ]
        for field, value in cases:
            manifest = copy.deepcopy(MANIFEST)
            manifest[field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                validate(manifest, contents(MANIFEST))
        with self.assertRaisesRegex(ValueError, "differs"):
            validate(MANIFEST, contents(MANIFEST), expected={**MANIFEST, "version": "1.2.4"})

    def test_exports_fail_closed(self):
        for key in (".", "./core", "./native"):
            manifest = copy.deepcopy(MANIFEST)
            del manifest["exports"][key]
            with self.subTest(missing=key), self.assertRaises(ValueError):
                validate(manifest, contents(MANIFEST))
        for entry in ("./dist/../escape.js", "./src/native.js", "./dist/native.node", "./dist//native.js"):
            manifest = copy.deepcopy(MANIFEST)
            manifest["exports"]["./native"]["import"] = entry
            with self.subTest(entry=entry), self.assertRaises(ValueError):
                validate(manifest, contents(MANIFEST))
        manifest = copy.deepcopy(MANIFEST)
        del manifest["exports"]["./core"]["types"]
        with self.assertRaises(ValueError):
            validate(manifest, contents(MANIFEST))

    def test_required_documents_and_native_library(self):
        for document in DOCUMENTS:
            for body in (None, b""):
                files = contents(MANIFEST)
                if body is None:
                    del files["package/" + document]
                else:
                    files["package/" + document] = body
                with self.subTest(document=document, body=body), self.assertRaises(ValueError):
                    validate(MANIFEST, files)
        for body in (b"", b"not ELF"):
            files = contents(MANIFEST)
            files["package/dist/libpulsebeam_agent_native.so"] = body
            with self.subTest(body=body), self.assertRaises(ValueError):
                validate(MANIFEST, files)
        files = contents(MANIFEST)
        del files["package/dist/libpulsebeam_agent_native.so"]
        with self.assertRaises(ValueError):
            validate(MANIFEST, files)


if __name__ == "__main__":
    unittest.main()
