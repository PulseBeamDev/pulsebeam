import argparse
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from native_build import native_build_lock, reapi_address, siso_command, writable_file


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


if __name__ == "__main__":
    unittest.main()
