import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import warnings
import zipfile

from release.pypi import release


VERSION = "1.2.3rc1"
TAG = "pypi-agent-v" + VERSION
PREFIX = "pulsebeam_agent-" + VERSION
WHEEL_NAME = PREFIX + "-py3-none-manylinux_2_39_x86_64.whl"
DIST_INFO = PREFIX + ".dist-info/"


def entries():
    return {
        "pulsebeam_agent/__init__.py": b"",
        "pulsebeam_agent/pulsebeam_agent_core.py": b"# generated",
        "pulsebeam_agent/pulsebeam_agent_native.py": b"# generated",
        "pulsebeam_agent/libpulsebeam_agent_native.so": b"\x7fELF synthetic",
        "pulsebeam_agent/LICENSE": b"license",
        DIST_INFO + "licenses/THIRD_PARTY_NOTICES.md": b"notices",
        DIST_INFO + "licenses/RUST_STANDARD_LIBRARY_NOTICES.html": b"rust notices",
        DIST_INFO + "METADATA": ("Metadata-Version: 2.3\nName: pulsebeam-agent\nVersion: " + VERSION + "\n\n").encode(),
        DIST_INFO + "WHEEL": b"Wheel-Version: 1.0\nRoot-Is-Purelib: false\nTag: py3-none-manylinux_2_39_x86_64\n\n",
        DIST_INFO + "RECORD": b"",
    }


class ContractTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.pyproject = self.root / "pyproject.toml"
        self.pyproject.write_text('[project]\nname = "pulsebeam-agent"\nversion = "' + VERSION + '"\n')
        self.plan = release.plan(self.pyproject, TAG)

    def wheel(self, contents=None, name=WHEEL_NAME, extras=()):
        path = self.root / name
        with warnings.catch_warnings():
            warnings.simplefilter("ignore", UserWarning)
            with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as archive:
                for key, value in (entries() if contents is None else contents).items():
                    archive.writestr(key, value)
                for key, value in extras:
                    archive.writestr(key, value)
        return path

    def cli(self, *args):
        return subprocess.run([sys.executable, str(Path(release.__file__)), *map(str, args)],
                              capture_output=True, text=True, check=False)

    def test_plan_and_cli(self):
        self.assertEqual(self.plan["intent"], "prerelease")
        self.assertEqual(self.plan["wheel_tag"], release.TAG)
        result = self.cli("plan", "--pyproject", self.pyproject, "--tag", TAG)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), self.plan)
        result = self.cli("validate", "--pyproject", self.pyproject, "--tag", TAG, "--wheel", self.wheel())
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), self.plan)
        result = self.cli("validate", "--pyproject", self.pyproject, "--tag", TAG, "--wheel", self.root)
        self.assertEqual(result.returncode, 0, result.stderr)
        result = self.cli("validate", "--pyproject", self.pyproject, "--tag", "bad", "--wheel", self.wheel())
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("tag does not match", result.stderr)

    def test_static_project_identity_and_versions(self):
        for project in [
            '[project]\nname="other"\nversion="1.2.3rc1"',
            '[project]\nname="pulsebeam-agent"\ndynamic=["version"]',
            '[project]\nname="pulsebeam-agent"\nversion="1.2.3rc1"\ndynamic=["version"]',
            '[project]\nname="pulsebeam-agent"\nversion="01.2"',
            '[project]\nname="pulsebeam-agent"\nversion="1.2+local"',
            '[project]\nname="pulsebeam-agent"\nversion="1.2.3rc1"\ndependencies=["other @ file:///tmp/local"]',
            'not toml',
        ]:
            with self.subTest(project=project):
                self.pyproject.write_text(project)
                with self.assertRaises(ValueError):
                    release.plan(self.pyproject, TAG)
        self.pyproject.write_text('[project]\nname="pulsebeam-agent"\nversion="1.2.3"')
        self.assertEqual(release.plan(self.pyproject, "pypi-agent-v1.2.3")["intent"], "stable")
        with self.assertRaises(ValueError):
            release.plan(self.pyproject, TAG)

    def test_required_files_and_license_locations(self):
        release.validate_wheel(self.wheel(), self.plan)
        files = entries()
        files[DIST_INFO + "licenses/LICENSE"] = files.pop("pulsebeam_agent/LICENSE")
        files["pulsebeam_agent/THIRD_PARTY_NOTICES.md"] = files.pop(DIST_INFO + "licenses/THIRD_PARTY_NOTICES.md")
        release.validate_wheel(self.wheel(files), self.plan)
        for missing in entries():
            with self.subTest(missing=missing):
                files = entries()
                del files[missing]
                with self.assertRaises(ValueError):
                    release.validate_wheel(self.wheel(files), self.plan)
        for name in ["pulsebeam_agent/LICENSE", DIST_INFO + "licenses/THIRD_PARTY_NOTICES.md",
                     DIST_INFO + "licenses/RUST_STANDARD_LIBRARY_NOTICES.html",
                     "pulsebeam_agent/libpulsebeam_agent_native.so", "pulsebeam_agent/pulsebeam_agent_core.py"]:
            with self.subTest(empty=name), self.assertRaises(ValueError):
                files = entries()
                files[name] = b""
                release.validate_wheel(self.wheel(files), self.plan)
        files = entries()
        files["THIRD_PARTY_NOTICES.md"] = files.pop(DIST_INFO + "licenses/THIRD_PARTY_NOTICES.md")
        with self.assertRaises(ValueError):
            release.validate_wheel(self.wheel(files), self.plan)

    def test_filename_identity(self):
        for name in ["other-1.2.3rc1-py3-none-manylinux_2_39_x86_64.whl",
                     "pulsebeam_agent-1.2.4-py3-none-manylinux_2_39_x86_64.whl",
                     PREFIX + "-py3-none-any.whl", PREFIX + "-py3-none-manylinux_2_17_x86_64.whl"]:
            with self.subTest(name=name), self.assertRaises(ValueError):
                release.validate_wheel(self.wheel(name=name), self.plan)
        release.validate_wheel(self.wheel(name=PREFIX + "-1-py3-none-manylinux_2_39_x86_64.whl"), self.plan)

    def test_metadata_identity_and_tags(self):
        changes = [
            ("METADATA", b"Metadata-Version: 2.3\nName: wrong\nVersion: 1.2.3rc1\n"),
            ("METADATA", b"Metadata-Version: 2.3\nName: pulsebeam-agent\nVersion: 1.2.3\n"),
            ("METADATA", b"Name: pulsebeam-agent\nVersion: 1.2.3rc1\n"),
            ("METADATA", b"Metadata-Version: 2.3\nName: pulsebeam-agent\nVersion: 1.2.3rc1\nRequires-Dist: other @ file:///tmp/local\n"),
            ("METADATA", b"Metadata-Version: 2.3\nName: pulsebeam-agent\nName: pulsebeam-agent\nVersion: 1.2.3rc1\n"),
            ("WHEEL", b"Wheel-Version: 1.0\nRoot-Is-Purelib: true\nTag: py3-none-any\n"),
            ("WHEEL", b"Wheel-Version: 1.0\nRoot-Is-Purelib: false\nTag: py3-none-manylinux_2_39_x86_64\nTag: py3-none-any\n"),
            ("WHEEL", b"Wheel-Version: 9.0\nRoot-Is-Purelib: false\nTag: py3-none-manylinux_2_39_x86_64\n"),
            ("WHEEL", b"not a header\n"),
        ]
        for name, data in changes:
            with self.subTest(data=data):
                files = entries()
                files[DIST_INFO + name] = data
                with self.assertRaises(ValueError):
                    release.validate_wheel(self.wheel(files), self.plan)

    def test_unsafe_and_duplicate_paths(self):
        for extra in ["../escape", "/absolute", "foo/../../escape", "a//b", "a/./b",
                      "C:/escape", "a\\b", "pulsebeam_agent/__init__.py",
                      "pulsebeam_agent/__init__.py/child", "pulsebeam_agent/__init__.py/",
                      "other-1.dist-info/WHEEL"]:
            with self.subTest(extra=extra), self.assertRaises(ValueError):
                release.validate_wheel(self.wheel(extras=[(extra, b"x")]), self.plan)
        link = zipfile.ZipInfo("pulsebeam_agent/link")
        link.create_system = 3
        link.external_attr = 0o120777 << 16
        with self.assertRaises(ValueError):
            release.validate_wheel(self.wheel(extras=[(link, b"target")]), self.plan)

    def test_malformed_archives(self):
        path = self.root / WHEEL_NAME
        path.write_bytes(b"not a zip")
        with self.assertRaises(ValueError):
            release.validate_wheel(path, self.plan)
        path = self.wheel()
        data = path.read_bytes()
        path.write_bytes(data[:-20])
        with self.assertRaises(ValueError):
            release.validate_wheel(path, self.plan)
        result = self.cli("validate", "--pyproject", self.pyproject, "--tag", TAG, "--wheel", path)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("invalid wheel archive", result.stderr)


if __name__ == "__main__":
    unittest.main()
