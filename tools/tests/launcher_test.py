import argparse
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
        self.root = Path(self.temp.name)
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
        self.env = dict(os.environ, XDG_CACHE_HOME=str(cache))

    def run_launcher(self, *arguments):
        return subprocess.run([str(self.launcher), *arguments], env=self.env,
                              capture_output=True)

    def test_normal_host_and_exact_argument_forwarding(self):
        arguments = ["--output_base=/tmp/with spaces", "test", "//:example", "--test_arg=quote ' \""]
        result = self.run_launcher(*arguments)
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        self.assertEqual(result.stdout.decode().split("\0")[:-1], ["1", *arguments])

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
