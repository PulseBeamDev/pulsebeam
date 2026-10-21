import importlib.util
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch


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

    def test_refresh_preserves_child_tool_runfiles(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            artifact = root / "artifact"
            (artifact / "dist").mkdir(parents=True)
            (artifact / "dist/index.js").write_text("export {}")
            (artifact / "lib.rs").write_text("// generated")
            runtime = root / "editor.runfiles"
            (runtime / "bin").mkdir(parents=True)
            (runtime / "runtime.txt").write_text("declared tool runtime")
            tool = root / "tool"
            tool.write_text(
                f"#!{sys.executable}\n"
                "from pathlib import Path\n"
                "runtime = Path(__file__).parent.parent / 'runtime.txt'\n"
                "(Path.cwd() / 'installed').write_text(runtime.read_text())\n"
            )
            tool.chmod(0o755)
            (runtime / "bin/pnpm").symlink_to(tool)
            packages = ("agents/pulsebeam-agent-web", "agents/react", "agents/react/tests/consumer", "apps/meet", "docs")
            for package in packages:
                (root / package).mkdir(parents=True, exist_ok=True)
            inputs = {name: artifact for name in ("web", "react", "bindings", "next", "native")}
            inputs.update({name: artifact / "lib.rs" for name in ("rust-lib", "web-module", "core-module", "entrypoint")})
            inputs["pnpm"] = runtime / "bin/pnpm"
            argv = ["ide"] + [arg for name, path in inputs.items() for arg in ("--" + name, str(path))]
            with patch.object(sys, "argv", argv), patch.dict(os.environ, {"BUILD_WORKSPACE_DIRECTORY": str(root)}):
                ide.main()
            for package in packages:
                self.assertEqual((root / package / "installed").read_text(), "declared tool runtime")
            self.assertEqual((root / "agents/pulsebeam-agent-web/dist/index.js").read_text(), "export {}")

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
