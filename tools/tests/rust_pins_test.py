import importlib.util
import io
import json
from pathlib import Path
import unittest
from unittest.mock import patch


spec = importlib.util.spec_from_file_location("rust_pins", Path(__file__).parents[1] / "update-rust-pins.py")
pins = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pins)


class RustArchivePinsTests(unittest.TestCase):
    def setUp(self):
        self.artifacts = []
        self.expected = {}
        for component in ["cargo", "clippy-preview", "llvm-tools-preview", "rust-analyzer-preview",
                          "rust-src", "rust-std", "rustc", "rustfmt-preview"]:
            targets = ["*"] if component == "rust-src" else ["x86_64-unknown-linux-gnu", "aarch64-unknown-linux-gnu"]
            if component == "rust-std":
                targets.append("wasm32-unknown-unknown")
            for target in targets:
                suffix = "" if target == "*" else "-" + target
                archive = f"{component.removesuffix('-preview')}-1.99.0{suffix}.tar.xz"
                digest = f"{len(self.artifacts):064x}"
                self.expected[archive] = digest
                self.artifacts.append(
                    f'[pkg.{json.dumps(component)}.target.{json.dumps(target)}]\n'
                    f'available = true\nxz_url = "https://static.rust-lang.org/dist/{archive}"\n'
                    f'xz_hash = "{digest}"\n'
                )

    def fetch(self, manifest):
        with patch.object(pins, "urlopen", return_value=io.BytesIO(manifest.encode())) as download:
            result = pins.rust_archive_hashes("1.99.0")
        download.assert_called_once_with("https://static.rust-lang.org/dist/channel-rust-1.99.0.toml")
        return result

    def test_archive_keys_cover_linux_hosts_wasm_and_shared_source(self):
        result = self.fetch("\n".join(reversed(self.artifacts)))
        self.assertEqual(result, self.expected)
        self.assertEqual(list(result), sorted(result))
        self.assertIn("rust-src-1.99.0.tar.xz", result)
        self.assertIn("rust-std-1.99.0-wasm32-unknown-unknown.tar.xz", result)
        self.assertIn("rustfmt-1.99.0-aarch64-unknown-linux-gnu.tar.xz", result)

    def test_unavailable_required_archive_fails(self):
        manifest = "\n".join(self.artifacts).replace("available = true", "available = false", 1)
        with self.assertRaisesRegex(ValueError, "lacks cargo for x86_64-unknown-linux-gnu"):
            self.fetch(manifest)

    def test_missing_required_archive_fails(self):
        with self.assertRaises(KeyError):
            self.fetch("\n".join(self.artifacts[1:]))

    def test_invalid_checksum_fails(self):
        manifest = "\n".join(self.artifacts).replace("0" * 64, "invalid", 1)
        with self.assertRaisesRegex(ValueError, "invalid Rust archive checksum"):
            self.fetch(manifest)


if __name__ == "__main__":
    unittest.main()
