import argparse
import hashlib
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest


class LauncherContractTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        host = self.root / "tools/host"
        host.mkdir(parents=True)
        shutil.copyfile(options.flake, host / "flake.nix")
        shutil.copyfile(options.lock, host / "flake.lock")
        arch = "amd64" if os.uname().machine == "x86_64" else "arm64"
        self.identity = hashlib.sha256(
            Path(options.flake).read_bytes() + Path(options.lock).read_bytes()
        ).hexdigest() + ":" + arch
        source = Path(options.launcher).read_text()
        cache_name = re.search(r"/pulsebeam/(bazelisk-[0-9.]+-)", source).group(1) + arch
        probe = b"#!/usr/bin/env bash\nprintf '%s\\0' \"$PULSEBEAM_HOST_ENV\" \"$BAZELISK_SKIP_WRAPPER\" \"$@\"\n"
        digest = hashlib.sha256(probe).hexdigest()
        launcher = self.root / "bazel"
        launcher.write_text(re.sub(r"sha=[0-9a-f]{64}", "sha=" + digest, source))
        launcher.chmod(0o755)
        cache = self.root / "cache"
        binary = cache / "pulsebeam" / cache_name / "bazelisk"
        binary.parent.mkdir(parents=True)
        binary.write_bytes(probe)
        binary.chmod(0o755)
        self.env = dict(os.environ, XDG_CACHE_HOME=str(cache),
                        PULSEBEAM_HOST_ACTIVE="1", PULSEBEAM_HOST_ENV="forged-identity")

    def test_verified_identity_and_exact_argument_forwarding(self):
        arguments = ["--output_base=/tmp/with spaces", "test", "//:example", "--test_arg=quote ' \""]
        result = subprocess.run([str(self.root / "bazel"), *arguments], env=self.env,
                                capture_output=True)
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        self.assertEqual(result.stdout.decode().split("\0")[:-1],
                         [self.identity, "1", "--host_jvm_args=-Dpulsebeam.host_env=" + self.identity,
                          *arguments])

    def test_stale_runtime_cannot_be_bypassed_by_environment_flags(self):
        with (self.root / "tools/host/flake.nix").open("a") as flake:
            flake.write("\n# Changed locked runtime input.\n")
        result = subprocess.run([str(self.root / "bazel"), "test"], env=self.env,
                                capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(b"Locked OS runtime changed", result.stderr)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--launcher", required=True)
    parser.add_argument("--flake", required=True)
    parser.add_argument("--lock", required=True)
    options, arguments = parser.parse_known_args()
    unittest.main(argv=[__file__, *arguments])
