import importlib.util
from pathlib import Path
import tempfile
import unittest


spec = importlib.util.spec_from_file_location("ide", Path(__file__).parents[1] / "ide.py")
ide = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ide)


class EditorMirrorTests(unittest.TestCase):
    def test_readonly_artifact_can_be_refreshed(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "artifact"
            nested = source / "nested"
            nested.mkdir(parents=True)
            (nested / "binding.ts").write_text("first")
            (nested / "binding.ts").chmod(0o444)
            nested.chmod(0o555)
            source.chmod(0o555)
            destination = root / "editor"
            try:
                ide.copy(source, destination)
                (destination / "nested/binding.ts").write_text("editor mirror")
                ide.copy(source, destination)
                self.assertEqual((destination / "nested/binding.ts").read_text(), "first")
                self.assertEqual((nested / "binding.ts").read_text(), "first")
            finally:
                ide.writable_directories(source)

    def test_stale_mirror_symlink_does_not_modify_its_target(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "artifact"
            source.mkdir()
            (source / "binding.ts").write_text("artifact")
            target = root / "unrelated"
            target.mkdir()
            (target / "keep").write_text("untouched")
            destination = root / "editor"
            destination.symlink_to(target, target_is_directory=True)
            ide.copy(source, destination)
            self.assertFalse(destination.is_symlink())
            self.assertEqual((destination / "binding.ts").read_text(), "artifact")
            self.assertEqual((target / "keep").read_text(), "untouched")


if __name__ == "__main__":
    unittest.main()
