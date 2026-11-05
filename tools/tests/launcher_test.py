import argparse
import fcntl
import hashlib
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest


class LauncherContractTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name) / "checkout with spaces"
        self.root.mkdir()
        arch = "amd64" if os.uname().machine == "x86_64" else "arm64"
        source = Path(options.launcher).read_text()
        cache_name = re.search(r"/pulsebeam/(bazelisk-[0-9.]+-)", source).group(1) + arch
        probe = b"#!/usr/bin/env bash\nprintf '%s\\0' \"$BAZELISK_SKIP_WRAPPER\" \"$@\"\n"
        digest = hashlib.sha256(probe).hexdigest()
        self.launcher = self.root / "bazel"
        self.launcher.write_text(re.sub(r"sha=[0-9a-f]{64}", "sha=" + digest, source))
        self.launcher.chmod(0o755)
        cache = self.root / "cache"
        self.binary = cache / "pulsebeam" / cache_name / "bazelisk"
        self.binary.parent.mkdir(parents=True)
        self.binary.write_bytes(probe)
        self.binary.chmod(0o755)
        home = self.root / "home"
        home.mkdir()
        self.env = dict(os.environ, HOME=str(home), XDG_CACHE_HOME=str(cache),
                        PULSEBEAM_NATIVELINK_AUTOSTART="1", AUTOSTART_LOG=str(self.root / "autostart.log"))
        helper = self.root / "tools/nativelink/autostart.py"
        helper.parent.mkdir(parents=True)
        helper.write_text("import os, sys\nfrom pathlib import Path\n"
                          "Path(os.environ['AUTOSTART_LOG']).write_text('\\n'.join(sys.argv[1:]))\n")

    def run_launcher(self, *arguments, cwd=None, launcher=None):
        return subprocess.run([str(launcher or self.launcher), *arguments], env=self.env,
                              cwd=cwd, capture_output=True)

    def forwarded_arguments(self, result):
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        return result.stdout.decode().split("\0")[:-1]

    @property
    def output_base(self):
        return f"--output_base={self.root}/.bazel/output"

    def test_default_output_stays_in_checkout_from_nested_directory(self):
        nested = self.root / "package"
        nested.mkdir()
        result = self.run_launcher("info", "output_base", cwd=nested)
        self.assertEqual(self.forwarded_arguments(result),
                         ["1", self.output_base, "info", "output_base"])

    def test_symlinked_launcher_uses_real_checkout(self):
        link = self.root.parent / "bazel-link"
        link.symlink_to(self.launcher)
        result = self.run_launcher("version", launcher=link)
        self.assertEqual(self.forwarded_arguments(result),
                         ["1", self.output_base, "version"])

    def test_normal_host_and_exact_argument_forwarding(self):
        arguments = ["--output_base=/tmp/with spaces", "test", "//:example", "--test_arg=quote ' \""]
        result = self.run_launcher(*arguments)
        self.assertEqual(self.forwarded_arguments(result),
                         ["1", self.output_base, *arguments])

    def test_retirement_lock_blocks_builds_without_leaving_per_worktree_files(self):
        key = hashlib.sha256(os.fsencode(str(self.root.resolve()))).hexdigest()[:2]
        lock = Path(self.env["HOME"]) / ".cache/dev/worktree-locks" / key
        lock.parent.mkdir(parents=True)
        with lock.open("w") as stream:
            fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
            result = self.run_launcher("test")
            self.assertEqual(result.returncode, 75)
        self.assertEqual(self.forwarded_arguments(self.run_launcher("version")),
                         ["1", self.output_base, "version"])
        self.assertEqual([entry.name for entry in lock.parent.iterdir()], [key])

    def test_build_commands_ensure_shared_service_without_changing_arguments(self):
        for command in ("build", "test", "run", "coverage"):
            with self.subTest(command=command):
                result = self.run_launcher(command, "//:example")
                self.assertEqual(self.forwarded_arguments(result), ["1", self.output_base, command, "//:example"])
                self.assertEqual(Path(self.env["AUTOSTART_LOG"]).read_text().splitlines(), [
                    "--workspace", str(self.root), "--bazelisk", str(self.binary),
                    "--output-base", str(self.root / ".bazel/output"),
                ])
                Path(self.env["AUTOSTART_LOG"]).unlink()

    def test_local_or_external_service_does_not_autostart(self):
        options = [
            ("info", "output_base"),
            ("test", "--config=local"),
            ("build", "--config", "local"),
            ("build", "--remote_executor=grpc://127.0.0.1:50052", "--remote_cache=grpc://127.0.0.1:50052",
             "--//crates/pulsebeam-libwebrtc-sys:reapi=grpc://127.0.0.1:50052"),
            ("run", "//tools/nativelink:server"),
        ]
        for args in options:
            with self.subTest(args=args):
                self.forwarded_arguments(self.run_launcher(*args))
                self.assertFalse(Path(self.env["AUTOSTART_LOG"]).exists())

    def test_local_webrtc_still_starts_bazel_coordinator(self):
        self.forwarded_arguments(self.run_launcher("build", "--//crates/pulsebeam-libwebrtc-sys:executor=local"))
        self.assertTrue(Path(self.env["AUTOSTART_LOG"]).exists())

    def test_manual_service_bootstraps_locally(self):
        self.assertEqual(self.forwarded_arguments(self.run_launcher("run", "//tools/nativelink:server")),
                         ["1", self.output_base, "run", "--config=local", "//tools/nativelink:server"])

    def test_autostart_failure_stops_bazel(self):
        helper = self.root / "tools/nativelink/autostart.py"
        helper.write_text("raise SystemExit('service failed')\n")
        result = self.run_launcher("build", "//:example")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"service failed", result.stderr)
        self.assertEqual(result.stdout, b"")

    def test_corrupt_cached_launcher_fails_actionably(self):
        self.binary.write_bytes(b"corrupt")
        result = self.run_launcher("test")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"Corrupt launcher: remove", result.stderr)
        self.assertIn(str(self.binary).encode(), result.stderr)

    def test_unsupported_host_fails_before_bootstrap(self):
        commands = self.root / "bin"
        commands.mkdir()
        uname = commands / "uname"
        uname.write_text("#!/usr/bin/env bash\necho Darwin\n")
        uname.chmod(0o755)
        self.env["PATH"] = str(commands) + os.pathsep + self.env["PATH"]
        result = self.run_launcher("test")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"requires a supported Linux host", result.stderr)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--launcher", required=True)
    options, arguments = parser.parse_known_args()
    unittest.main(argv=[__file__, *arguments])
