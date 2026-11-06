import argparse
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

from native_build import native_build_lock, reapi_address, repository_sources, siso_command, writable_file
from source_manifest import prepare


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
        self.assertFalse(any(arg.startswith("-reapi_") for arg in command))

    def test_execution_requires_scheduler_no_silent_local_fallback(self):
        command = siso_command("siso", "out/Default", "reapi", "127.0.0.1:50051")
        self.assertIn("-strict_remote", command)
        self.assertIn("-reapi_address=127.0.0.1:50051", command)
        self.assertNotIn("-offline", command)
        self.assertNotIn("-re_cache_enable_write", command)

    def test_cache_only_uploads_local_results(self):
        command = siso_command("siso", "out/Default", "cache", "127.0.0.1:50051")
        self.assertIn("-re_exec_enable=false", command)
        self.assertIn("-re_cache_enable_write", command)

    def test_endpoint_is_explicitly_loopback_only(self):
        self.assertEqual(reapi_address("grpc://127.0.0.1:50051"), "127.0.0.1:50051")
        for value in ("grpc://host:50051", "https://localhost:50051", "grpc://localhost", "grpc://localhost:50051/path", "grpc://user@localhost:50051", "grpc://localhost:50051?foo"):
            with self.subTest(value=value), self.assertRaises(argparse.ArgumentTypeError):
                reapi_address(value)

    def test_writable_overlay_does_not_modify_editable_upstream(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "upstream"
            (source / "config").mkdir(parents=True)
            original = source / "config/backend.star"
            original.write_text("upstream")
            stage = root / "stage"
            stage.mkdir()
            (stage / "config").symlink_to(source / "config", target_is_directory=True)
            writable_file(stage, "config/backend.star")
            (stage / "config/backend.star").write_text("local")
            self.assertEqual(original.read_text(), "upstream")

    def test_rustfmt_config_is_copied_into_source_view(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "upstream.toml"
            source.write_text('ignore = ["third_party/**"]\n')
            stage = root / "stage"
            stage.mkdir()
            config = stage / ".rustfmt.toml"
            config.symlink_to(source)
            writable_file(stage, ".rustfmt.toml")
            self.assertFalse(config.is_symlink())
            self.assertEqual(config.read_text(), source.read_text())


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
            for name in (".git/config", "resources/data", "nested/__pycache__/a.pyc", "nested/lib/systemd/unit", "nested/bytecode.pyc"):
                path = source / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("metadata")
            prepare(source, manifest)
            baseline = manifest.read_text()
            self.assertFalse((source / ".git").exists())
            self.assertFalse((source / "resources").exists())
            prepare(source, manifest)
            self.assertEqual(manifest.read_text(), baseline)
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

    def test_aliases_match_the_previous_regular_file_view(self):
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
            self.assertEqual((root / "actual.sha256").read_text(), (root / "expected.sha256").read_text())
            self.assertFalse(any(path.is_symlink() for path in source.rglob("*")))
            self.assertTrue(os.path.samefile(source / "file_alias", header))
            self.assertTrue(os.path.samefile(source / "alias/sub/header.h", header))
            self.assertEqual((source / "alias/relative").read_text(), "external")
            prepare(source, root / "again.sha256")
            self.assertEqual((root / "actual.sha256").read_text(), (root / "again.sha256").read_text())

    def test_escaping_metadata_and_cyclic_links_are_rejected(self):
        for target in ("/tmp", "../outside", "resources/data", "."):
            with self.subTest(target=target), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                source = root / "source"
                (source / "resources").mkdir(parents=True)
                (source / "resources/data").write_text("excluded")
                (source / "alias").symlink_to(target)
                with self.assertRaises(ValueError):
                    prepare(source, root / "manifest")
                self.assertFalse((root / "manifest").exists())


if __name__ == "__main__":
    unittest.main()
