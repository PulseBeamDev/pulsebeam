import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

from native_build import check_cxx_abi, native_build_lock, reapi_address, repository_sources, runtime_objects, siso_command, stage_file
from source_manifest import prepare


class CxxAbiPreflightTest(unittest.TestCase):
    def fixture(self, root, owned_version, sdk_version, generator_version=None):
        owned = root / "owned/Cargo.toml"
        owned.parent.mkdir()
        owned.write_text(f'[dependencies.cxx]\nversion = "={owned_version}"\n')
        sdk_root = root / "sdk"
        vendor = sdk_root / "third_party/rust/chromium_crates_io/vendor"
        for crate, version in (("cxx", sdk_version), ("cxxbridge-cmd", generator_version or sdk_version)):
            sdk = vendor / (crate + "-v1") / "Cargo.toml"
            sdk.parent.mkdir(parents=True)
            sdk.write_text(f'[package]\nname = "{crate}"\nversion = "{version}"\n')
        return owned, sdk_root

    def test_matching_runtime_and_generator_admit_build(self):
        with tempfile.TemporaryDirectory() as directory:
            owned, sdk_root = self.fixture(Path(directory), "1.0.205", "1.0.205")
            check_cxx_abi(owned, sdk_root)

    def test_runtime_mismatch_fails_with_owned_and_sdk_versions(self):
        with tempfile.TemporaryDirectory() as directory:
            owned, sdk_root = self.fixture(Path(directory), "1.0.194", "1.0.205")
            with self.assertRaisesRegex(SystemExit, "owned Rust cxx pin 1.0.194.*SDK CXX runtime version 1.0.205"):
                check_cxx_abi(owned, sdk_root)

    def test_generator_mismatch_fails_before_build(self):
        with tempfile.TemporaryDirectory() as directory:
            owned, sdk_root = self.fixture(Path(directory), "1.0.205", "1.0.205", "1.0.194")
            with self.assertRaisesRegex(SystemExit, "owned Rust cxx pin 1.0.205.*SDK CXX generator version 1.0.194"):
                check_cxx_abi(owned, sdk_root)


class NativeBuildLockTest(unittest.TestCase):
    def test_contention_fails_and_release_allows_retry(self):
        with tempfile.TemporaryDirectory(dir="/dev/shm") as directory:
            path = Path(directory) / "lock"
            with native_build_lock(path):
                with self.assertRaisesRegex(SystemExit, "another WebRTC build is running"):
                    with native_build_lock(path):
                        self.fail("a concurrent build acquired the lock")
            with native_build_lock(path):
                pass

    def test_child_keeps_lock_after_parent_closes_descriptor(self):
        with tempfile.TemporaryDirectory(dir="/dev/shm") as directory:
            path = Path(directory) / "lock"
            read_fd, write_fd = os.pipe()
            try:
                with native_build_lock(path) as lock_fd:
                    child = subprocess.Popen(
                        [sys.executable, "-c", f"import os; os.read({read_fd}, 1)"],
                        pass_fds=(lock_fd, read_fd),
                    )
                try:
                    with self.assertRaises(SystemExit):
                        with native_build_lock(path):
                            self.fail("a child build lost its lock")
                finally:
                    os.close(write_fd)
                    write_fd = -1
                    child.wait(timeout=5)
                self.assertEqual(child.returncode, 0)
                with native_build_lock(path):
                    pass
            finally:
                os.close(read_fd)
                if write_fd != -1:
                    os.close(write_fd)


class SisoCommandTest(unittest.TestCase):
    def test_local_has_no_reapi_or_fallback(self):
        command = siso_command("siso", "out/Default", "local", "127.0.0.1:50051")
        self.assertIn("-offline", command)
        self.assertIn("-load=@config//pulsebeam.star", command)
        self.assertNotIn("-config=remote-link", command)
        self.assertFalse(any(arg.startswith("-reapi_") for arg in command))

    def test_execution_requires_scheduler_no_silent_local_fallback(self):
        command = siso_command("siso", "out/Default", "reapi", "127.0.0.1:50051")
        self.assertIn("-strict_remote", command)
        self.assertIn("-k=8", command)
        self.assertIn("-config=remote-link", command)
        self.assertIn("-reapi_address=127.0.0.1:50051", command)
        self.assertNotIn("-offline", command)
        self.assertNotIn("-re_cache_enable_write", command)

    def test_cache_only_uploads_local_results(self):
        command = siso_command("siso", "out/Default", "cache", "127.0.0.1:50051")
        self.assertIn("-re_exec_enable=false", command)
        self.assertIn("-re_cache_enable_write", command)
        self.assertNotIn("-config=remote-link", command)

    def test_endpoint_is_explicitly_loopback_only(self):
        self.assertEqual(reapi_address("grpc://127.0.0.1:50051"), "127.0.0.1:50051")
        for value in ("grpc://host:50051", "https://localhost:50051", "grpc://localhost", "grpc://localhost:50051/path", "grpc://user@localhost:50051", "grpc://localhost:50051?foo"):
            with self.subTest(value=value), self.assertRaises(argparse.ArgumentTypeError):
                reapi_address(value)

    def test_owned_overlay_cannot_replace_upstream_inputs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "upstream"
            (source / "config").mkdir(parents=True)
            original = source / "config/backend.star"
            original.write_text("upstream")
            stage = root / "stage"
            stage.mkdir()
            (stage / "config").symlink_to(source / "config", target_is_directory=True)
            stage_file(stage, "config/pulsebeam.star", b"owned")
            self.assertEqual((stage / "config/pulsebeam.star").read_bytes(), b"owned")
            with self.assertRaisesRegex(ValueError, "shadows upstream"):
                stage_file(stage, "config/backend.star", b"replacement")
            self.assertEqual(original.read_text(), "upstream")
            self.assertEqual((stage / "config/backend.star").read_text(), "upstream")
            self.assertFalse((source / "config/pulsebeam.star").exists())

    def test_rustfmt_config_is_copied_into_source_view(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "upstream.toml"
            source.write_text('ignore = ["third_party/**"]\n')
            stage = root / "stage"
            stage.mkdir()
            config = stage / ".rustfmt.toml"
            config.symlink_to(source)
            stage_file(stage, ".rustfmt.toml")
            self.assertFalse(config.is_symlink())
            self.assertEqual(config.read_text(), source.read_text())


class RuntimeObjectsTest(unittest.TestCase):
    def test_stock_runtime_graph_handles_static_and_asan_source_sets(self):
        for abi_type in ("static_library", "source_set"):
            with self.subTest(abi_type=abi_type):
                graph = {
                    "//pulsebeam:runtime": {"type": "executable", "deps": ["cpp", "abi", "atomic"], "source_outputs": {"main.cc": ["obj/main.o"]}},
                    "cpp": {"type": "static_library", "deps": ["abi", "headers"], "source_outputs": {"cpp.cc": ["obj/cpp.o", "obj/cpp.d"]}},
                    "abi": {"type": abi_type, "source_outputs": {"abi.cc": ["obj/abi.o"]}},
                    "atomic": {"type": "source_set", "deps": ["cpp"], "source_outputs": {"atomic.c": ["obj/atomic.o"]}},
                    "headers": {"type": "action", "deps": ["generator"]},
                    "generator": {"type": "executable", "source_outputs": {"main.cc": ["host/main.o"]}},
                }
                self.assertEqual(runtime_objects(graph), ["obj/abi.o", "obj/atomic.o", "obj/cpp.o"])
                graph["abi"]["source_outputs"] = {"abi.cc": ["../outside.o"]}
                with self.assertRaisesRegex(ValueError, "escapes build directory"):
                    runtime_objects(graph)
                graph["abi"]["type"] = "shared_library"
                with self.assertRaisesRegex(ValueError, "not statically packageable"):
                    runtime_objects(graph)
                with self.assertRaisesRegex(ValueError, "no C\\+\\+ runtime objects"):
                    runtime_objects({"//pulsebeam:runtime": {"type": "group"}})


class SourceManifestTest(unittest.TestCase):
    def test_repository_sources_follow_staged_manifest_to_immutable_checkout(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "repository/checkout/src"
            source.mkdir(parents=True)
            (source / "code.cc").write_text("code")
            manifest = source.parent.parent / "sources.sha256"
            prepare(source, manifest)
            staged = root / "sandbox/sources.sha256"
            staged.parent.mkdir()
            staged.symlink_to(manifest)
            self.assertEqual(repository_sources(staged), source)
            self.assertEqual(repository_sources(manifest), source)
            for materialize in (shutil.copyfile, os.link):
                staged.unlink()
                materialize(manifest, staged)
                with self.assertRaisesRegex(argparse.ArgumentTypeError, "copied/hardlinked"):
                    repository_sources(staged)

    def test_identity_tracks_inputs_not_excluded_metadata(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            source.mkdir()
            code = source / "code.cc"
            code.write_text("code")
            manifest = root / "manifest"
            metadata = (".git/config", "nested/__pycache__/a.pyc", "nested/lib/systemd/unit", "nested/bytecode.pyc")
            for name in metadata:
                path = source / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("metadata")
            resource = source / "resources/data"
            resource.parent.mkdir()
            resource.write_text("resource")
            prepare(source, manifest)
            baseline = manifest.read_text()
            for name in metadata:
                self.assertEqual((source / name).read_text(), "metadata")
                (source / name).write_text("changed metadata")
            prepare(source, manifest)
            self.assertEqual(manifest.read_text(), baseline)
            resource.write_text("changed resource")
            prepare(source, manifest)
            self.assertNotEqual(manifest.read_text(), baseline)
            baseline = manifest.read_text()
            code.chmod(0o600)
            prepare(source, manifest)
            self.assertEqual(manifest.read_text(), baseline)
            code.chmod(0o700)
            prepare(source, manifest)
            self.assertNotEqual(manifest.read_text(), baseline)
            executable = manifest.read_text()
            code.write_text("changed")
            prepare(source, manifest)
            self.assertNotEqual(manifest.read_text(), executable)

    def test_aliases_are_identified_without_changing_upstream_nodes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            (source / "target/sub").mkdir(parents=True)
            header = source / "target/sub/header.h"
            header.write_text("header")
            header.chmod(0o751)
            (source / "external").mkdir()
            (source / "external/header.h").write_text("external")
            (source / "target/relative").symlink_to("../external/header.h")
            (source / "alias").symlink_to("target", target_is_directory=True)
            (source / "file_alias").symlink_to("target/sub/header.h")
            expected = root / "expected"
            shutil.copytree(source, expected, symlinks=False)
            prepare(expected, root / "expected.sha256")
            (source / "dangling").symlink_to("absent")
            prepare(source, root / "actual.sha256")
            self.assertNotEqual((root / "actual.sha256").read_text(), (root / "expected.sha256").read_text())
            self.assertTrue((source / "dangling").is_symlink())
            self.assertEqual(os.readlink(source / "alias"), "target")
            self.assertEqual(os.readlink(source / "file_alias"), "target/sub/header.h")
            self.assertEqual(header.stat().st_mode & 0o777, 0o751)
            self.assertTrue(os.path.samefile(source / "alias/sub/header.h", header))
            self.assertEqual((source / "alias/relative").read_text(), "external")
            prepare(source, root / "again.sha256")
            self.assertEqual((root / "actual.sha256").read_text(), (root / "again.sha256").read_text())

    def test_escaping_metadata_and_cyclic_links_are_rejected(self):
        for target in ("/tmp", "../outside", ".git/config", "."):
            with self.subTest(target=target), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                source = root / "source"
                (source / ".git").mkdir(parents=True)
                (source / ".git/config").write_text("excluded")
                (source / "alias").symlink_to(target)
                with self.assertRaises(ValueError):
                    prepare(source, root / "manifest")
                self.assertFalse((root / "manifest").exists())


if __name__ == "__main__":
    unittest.main()
