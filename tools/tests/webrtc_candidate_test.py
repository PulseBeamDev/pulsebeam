import hashlib
import importlib.util
import io
import json
from pathlib import Path
import sys
import tarfile
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).parents[1]))
from webrtc_candidate import validate

spec = importlib.util.spec_from_file_location("prepare", Path(__file__).parents[1] / "prepare-webrtc-candidate.py")
prepare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prepare)


class CandidateTests(unittest.TestCase):
    def test_wrong_archive_checksum_does_not_extract(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "archive.tar.gz"
            archive.write_bytes(b"not the pinned archive")
            with self.assertRaisesRegex(ValueError, "checksum mismatch"):
                prepare.unpack(archive, "0" * 64, root / "output")
            self.assertFalse((root / "output").exists())

    def test_archive_aliases_traversal_and_file_ancestors_are_rejected(self):
        for names in [["x", "./x"], ["../escape"], ["x", "x/child"], ["x/child", "x"]]:
            with self.subTest(names=names), tempfile.TemporaryDirectory() as temporary:
                root = Path(temporary)
                archive = root / "archive.tar.gz"
                with tarfile.open(archive, "w:gz") as bundle:
                    for name in names:
                        member = tarfile.TarInfo(name)
                        member.size = 1
                        bundle.addfile(member, io.BytesIO(b"x"))
                digest = hashlib.sha256(archive.read_bytes()).hexdigest()
                with self.assertRaises(ValueError):
                    prepare.unpack(archive, digest, root / "output")
                self.assertFalse((root / "output").exists())

    def test_source_revision_is_checked_before_extraction(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            archive = root / "source.tar.gz"
            with tarfile.open(archive, "w:gz", format=tarfile.PAX_FORMAT, pax_headers={"comment": "a" * 40}) as bundle:
                member = tarfile.TarInfo("Cargo.toml")
                member.size = 1
                bundle.addfile(member, io.BytesIO(b"x"))
            digest = hashlib.sha256(archive.read_bytes()).hexdigest()
            with self.assertRaisesRegex(ValueError, "revision mismatch"):
                prepare.unpack(archive, digest, root / "output", "b" * 40)

    def test_native_bridge_identity_mismatch_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            manifest = json.loads(NATIVE_MANIFEST.read_text())
            manifest["bridge"]["identity"] = "not-the-source-bridge"
            (root / "manifest.json").write_text(json.dumps(manifest))
            with self.assertRaisesRegex(ValueError, "bridge identity mismatch"):
                validate(SOURCE_MANIFEST.parent, root)

    def test_native_library_checksum_mismatch_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "manifest.json").write_bytes(NATIVE_MANIFEST.read_bytes())
            (root / "include").symlink_to(NATIVE_MANIFEST.parent / "include", target_is_directory=True)
            (root / "lib").mkdir()
            (root / "lib/libwebrtc.a").write_bytes(b"not the matching native library")
            with self.assertRaisesRegex(ValueError, "library checksum mismatch"):
                validate(SOURCE_MANIFEST.parent, root)


if __name__ == "__main__":
    SOURCE_MANIFEST = Path(sys.argv.pop(1)).resolve()
    NATIVE_MANIFEST = Path(sys.argv.pop(1)).resolve()
    unittest.main()
