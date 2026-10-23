import importlib.util
import io
import json
from pathlib import Path
import re
import tempfile
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


class NativeTracingPinsTests(unittest.TestCase):
    def setUp(self):
        self.lock = [
            {"name": "tracing", "version": "1.2.3", "source": "registry+example"},
            {"name": "tracing-core", "version": "4.5.6", "source": "registry+example"},
        ]

    def test_matching_identities_share_application_tracing_and_its_core(self):
        self.assertEqual(
            pins.native_tracing_override(self.lock, self.lock + [{"name": "unrelated", "version": "1"}]),
            'crate.annotation(crate = "tracing", version = "=1.2.3", '
            'override_target_lib = "@crates//:tracing", repositories = ["str0m_native"])\n',
        )

    def test_missing_or_ambiguous_identity_is_not_silently_overridden(self):
        for index in range(2):
            name = self.lock[index]["name"]
            for changed in (self.lock[:index] + self.lock[index + 1:], self.lock + [self.lock[index]]):
                for application, native in ((changed, self.lock), (self.lock, changed)):
                    with self.subTest(name=name, application=application, native=native):
                        with self.assertRaisesRegex(ValueError, f"shared native {name}"):
                            pins.native_tracing_override(application, native)

    def test_version_or_source_drift_is_not_silently_overridden(self):
        for index in range(2):
            for key in ("version", "source"):
                changed = [p.copy() for p in self.lock]
                changed[index][key] = "different"
                with self.subTest(name=changed[index]["name"], key=key):
                    with self.assertRaisesRegex(ValueError, f'shared native {changed[index]["name"]}'):
                        pins.native_tracing_override(self.lock, changed)


class RtcVisibilityTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "tools/patches").mkdir(parents=True)
        self.source = self.root / "upstream"
        self.files = {
            "src/lib.rs": b'pub mod crypto;\nmod dtls;\npub(crate) mod change;\nmod format;\nmod sdp;\n#[path = "rtp/mod.rs"]\nmod rtp_;\nmod unrelated;\npub(crate) use untouched;\n',
            "src/dtls.rs": b'pub(crate) struct Dtls;\n',
            "src/crypto/mod.rs": b'pub(crate) use keys::*;\nmod keys;\npub mod public;\n',
            "src/change/mod.rs": b'\tpub(crate) fn change() {}\nmod test {\n    mod nested {}\n}\n',
            "src/format/mod.rs": b'// mod untouched; pub(crate) docs\nconst TEXT: &str = "pub(crate)";\n',
            "src/sdp/mod.rs": b'pub(crate)  struct Sdp;\r\n',
            "src/rtp/mod.rs": b'#[path = "elsewhere.rs"]\nmod child;\n',
            "src/rtp/rtcp/header.rs": b'    pub(crate) struct Header;',
            "src/unrelated.rs": b'pub(crate) struct Private;\nmod child;\n',
        }
        for name, data in self.files.items():
            path = self.source / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
        self.spec = {"git": "https://example.test/str0m.git", "rev": "abc123"}
        self.source_id = "git+https://example.test/str0m.git?rev=abc123#abc123"
        self.lock = [{"name": name, "version": version, "source": self.source_id}
                     for name, version in [("str0m", "9.8.7"), ("is", "1.2.3")]]
        self.metadata = {"packages": [self.lock[0] | {"manifest_path": str(self.source / "Cargo.toml")} ]}

    def apply_patch(self, patch):
        # A strict fixture-only unified-diff reader checks every consumed source line.
        lines = patch.splitlines(keepends=True)
        lines = [line[:-1] if i + 1 < len(lines) and lines[i + 1].startswith(b"\\ No newline") else line
                 for i, line in enumerate(lines) if not line.startswith(b"\\ No newline")]
        i = 0
        while i < len(lines):
            self.assertTrue(lines[i].startswith(b"diff --git a/"))
            i += 1
            self.assertTrue(lines[i].startswith(b"--- a/"))
            relative = lines[i][6:].strip().decode()
            self.assertEqual(lines[i + 1], b"+++ b/" + relative.encode() + b"\n")
            path = self.source / relative
            before = path.read_bytes().splitlines(keepends=True)
            output = []
            cursor = 0
            i += 2
            while i < len(lines) and lines[i].startswith(b"@@"):
                header = re.fullmatch(rb"@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@\n", lines[i])
                self.assertIsNotNone(header)
                start = int(header[1]) - 1
                output.extend(before[cursor:start])
                cursor = start
                i += 1
                while i < len(lines) and not lines[i].startswith((b"@@", b"diff --git a/")):
                    kind, content = lines[i][:1], lines[i][1:]
                    if kind in (b" ", b"-"):
                        self.assertEqual(before[cursor], content)
                        cursor += 1
                    if kind in (b" ", b"+"):
                        output.append(content)
                    i += 1
            path.write_bytes(b"".join(output + before[cursor:]))

    def test_patch_application_preserves_scope_bytes_and_is_idempotent(self):
        patch = pins.str0m_visibility_patch(self.source)
        self.assertEqual(patch, pins.str0m_visibility_patch(self.source))
        for name, data in self.files.items():
            self.assertEqual((self.source / name).read_bytes(), data)
        self.apply_patch(patch)
        expected = self.files | {
            "src/lib.rs": self.files["src/lib.rs"].replace(b"mod dtls;", b"pub mod dtls;").replace(b"pub(crate) mod", b"pub mod").replace(b"\nmod format;", b"\npub mod format;").replace(b"\nmod sdp;", b"\npub mod sdp;").replace(b"\nmod rtp_;", b"\npub mod rtp_;"),
            "src/dtls.rs": b'pub struct Dtls;\n',
            "src/crypto/mod.rs": b'pub use keys::*;\npub mod keys;\npub mod public;\n',
            "src/change/mod.rs": b'\tpub fn change() {}\npub mod test {\n    pub mod nested {}\n}\n',
            "src/sdp/mod.rs": b'pub  struct Sdp;\r\n',
            "src/rtp/mod.rs": b'#[path = "elsewhere.rs"]\npub mod child;\n',
            "src/rtp/rtcp/header.rs": b'    pub struct Header;',
        }
        for name, data in expected.items():
            self.assertEqual((self.source / name).read_bytes(), data, name)
        self.assertEqual(pins.str0m_visibility_patch(self.source), b"")

    def test_missing_required_structure_fails(self):
        for relative in ("src/lib.rs", "src/dtls.rs", "src/crypto/mod.rs", "src/change/mod.rs",
                         "src/format/mod.rs", "src/sdp/mod.rs", "src/rtp/mod.rs"):
            with self.subTest(relative=relative):
                path = self.source / relative
                path.unlink()
                # rtp has a nested file, so remove its tree for the missing-tree case.
                nested = self.source / "src/rtp/rtcp/header.rs"
                if relative == "src/rtp/mod.rs":
                    nested.unlink()
                with self.assertRaisesRegex(ValueError, "str0m visibility: missing"):
                    pins.str0m_visibility_patch(self.source)
                path.write_bytes(self.files[relative])
                if relative == "src/rtp/mod.rs":
                    nested.write_bytes(self.files["src/rtp/rtcp/header.rs"])

    def test_missing_or_ambiguous_root_declarations_fail(self):
        path = self.source / "src/lib.rs"
        for module in ("crypto", "dtls", "change", "format", "sdp", "rtp_"):
            original = path.read_bytes()
            for data in (re.sub(rb"^.*mod " + module.encode() + rb";\n", b"", original, flags=re.MULTILINE),
                         original + f"mod {module};\n".encode()):
                path.write_bytes(data)
                with self.assertRaisesRegex(ValueError, f"expected one root declaration for {module}"):
                    pins.str0m_visibility_patch(self.source)
            path.write_bytes(original)

    def test_resolved_identity_selects_source_and_changed_source_changes_patch(self):
        deps = {name: self.spec for name in ("str0m", "is")}
        decoy = self.lock[0] | {"source": "git+https://example.test/other#abc123", "manifest_path": "/not/the/rtc/source"}
        self.metadata["packages"].insert(0, decoy)
        selected, changed = pins.rtc_visibility(self.root, self.lock, deps, self.metadata)
        output = self.root / "tools/patches/str0m-rtc-visibility.patch"
        first = output.read_bytes()
        self.assertEqual(selected, self.lock[0])
        self.assertTrue(changed)
        pins.rtc_visibility(self.root, self.lock, deps, self.metadata)
        self.assertEqual(output.read_bytes(), first)
        (self.source / "src/dtls.rs").write_bytes(b'pub(crate) struct NewUpstream;\n')
        pins.rtc_visibility(self.root, self.lock, deps, self.metadata)
        self.assertNotEqual(output.read_bytes(), first)
        self.apply_patch(output.read_bytes())
        self.assertEqual((self.source / "src/dtls.rs").read_bytes(), b'pub struct NewUpstream;\n')

    def test_revision_and_branch_identities_and_resolution_failures(self):
        self.assertEqual(pins.locked_package(self.lock, "str0m", self.spec), self.lock[0])
        branch = {"git": self.spec["git"], "branch": "feat/native"}
        package = self.lock[0] | {"source": f'git+{branch["git"]}?branch=feat%2Fnative#abc123'}
        self.assertEqual(pins.locked_package(self.lock + [package], "str0m", branch), package)
        for lock in ([], self.lock + [self.lock[0]]):
            with self.assertRaisesRegex(ValueError, "expected one locked str0m"):
                pins.locked_package(lock, "str0m", self.spec)
        deps = {name: self.spec for name in ("str0m", "is")}
        for packages in ([], self.metadata["packages"] * 2):
            with self.assertRaisesRegex(ValueError, "expected one resolved RTC str0m manifest"):
                pins.rtc_visibility(self.root, self.lock, deps, {"packages": packages})
        other = {"git": self.spec["git"], "rev": "different"}
        with self.assertRaisesRegex(ValueError, "must resolve from the same Git source"):
            pins.rtc_visibility(self.root, [self.lock[0], self.lock[1] | {"source": self.source_id.replace("abc123", "different")}],
                                deps | {"is": other}, self.metadata)


if __name__ == "__main__":
    unittest.main()
