import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import unittest

import config
from server import sandbox_script


def modified(properties, modifications):
    """Model v1.8.3 PropertyModifierScheduler's ordered map edits."""
    result = dict(properties)
    for item in modifications:
        if "remove" in item:
            result.pop(item["remove"], None)
        elif "add" in item:
            add = item["add"]
            result[add["name"]] = add["value"]
        else:
            replace = item["replace"]
            value = result.pop(replace["name"], None)
            if value is not None:
                result[replace["new_name"]] = replace.get("new_value", value)
    return result


class ConfigTest(unittest.TestCase):
    def setUp(self):
        self.cache_bytes = 32 * 1024**3
        self.config = config.make_config("/tmp/pulsebeam-test", 8, 8_388_608, 1, 1_048_576, 50051, 50061, cache_bytes=self.cache_bytes)

    def test_topology_and_bounds(self):
        c = self.config
        self.assertEqual([s["listener"]["http"]["socket_address"] for s in c["servers"]],
                         ["127.0.0.1:50051", "127.0.0.1:50061"])
        self.assertEqual(set(c["servers"][0]["services"]), {"cas", "ac", "bytestream", "execution", "capabilities"})
        self.assertEqual(set(c["servers"][1]["services"]), {"worker_api", "health"})
        cas, ac, fast = c["stores"]
        cas_bytes = cas["filesystem"]["eviction_policy"]["max_bytes"]
        self.assertEqual(cas_bytes, self.cache_bytes)
        self.assertEqual(ac["completeness_checking"]["backend"]["filesystem"]["eviction_policy"]["max_bytes"], cas_bytes // 100)
        self.assertEqual(fast["fast_slow"]["fast"]["filesystem"]["eviction_policy"]["max_bytes"], cas_bytes)
        self.assertEqual(fast["fast_slow"]["slow"]["ref_store"]["name"], cas["name"])
        self.assertEqual(len(c["workers"]), 1)
        worker = c["workers"][0]["local"]
        self.assertEqual(worker["worker_api_endpoint"]["uri"], "grpc://127.0.0.1:50061")
        self.assertEqual(worker["cas_fast_slow_store"], fast["name"])
        self.assertEqual(worker["max_concurrent_input_downloads"], 64)
        self.assertNotIn("experimental_active_input_leases", worker)
        self.assertEqual(worker["upload_action_result"]["ac_store"], ac["name"])
        self.assertEqual(worker["platform_properties"]["cpu_count"]["values"], ["8"])
        self.assertEqual(worker["platform_properties"]["memory_kb"]["values"], ["8388608"])
        scheduler = c["schedulers"][0]
        self.assertEqual(c["servers"][0]["services"]["execution"][0]["scheduler"], scheduler["name"])
        self.assertEqual(c["servers"][1]["services"]["worker_api"]["scheduler"], scheduler["name"])
        lookup = scheduler["property_modifier"]["scheduler"]["cache_lookup"]
        self.assertEqual(lookup["ac_store"], ac["name"])
        self.assertEqual(ac["completeness_checking"]["cas_store"]["ref_store"]["name"], cas["name"])
        simple = lookup["scheduler"]["simple"]
        self.assertEqual(simple["worker_timeout_s"], 120)
        self.assertNotIn("max_action_executing_timeout_s", simple)
        self.assertNotIn("timeout", worker["worker_api_endpoint"])
        props = simple["supported_platform_properties"]
        self.assertEqual((props["cpu_count"], props["memory_kb"]), ("minimum", "minimum"))

    def test_default_and_explicit_reservations(self):
        edits = self.config["schedulers"][0]["property_modifier"]["modifications"]
        for initial, expected in [
            ({}, {"cpu_count": "1", "memory_kb": "1048576"}),
            ({"cpu_count": "4"}, {"cpu_count": "4", "memory_kb": "1048576"}),
            ({"memory_kb": "2097152"}, {"cpu_count": "1", "memory_kb": "2097152"}),
            ({"cpu_count": "2", "memory_kb": "3", "OSFamily": "Linux"}, {"cpu_count": "2", "memory_kb": "3", "OSFamily": "Linux"}),
            ({"pulsebeam_saved_cpu_count": "0", "pulsebeam_saved_memory_kb": "0"}, {"cpu_count": "1", "memory_kb": "1048576"}),
        ]:
            with self.subTest(initial=initial):
                self.assertEqual(modified(initial, edits), expected)

    def test_invalid_parameters(self):
        args = ["/tmp/state", 2, 100, 1, 10, 50051, 50061, self.cache_bytes]
        for index, value in [(0, "relative"), (0, "/tmp/$HOME"), (1, 0), (2, -1), (3, 3), (4, 101), (5, 0), (6, 65536), (6, 50051), (7, 0), (7, -1)]:
            candidate = args.copy()
            candidate[index] = value
            with self.subTest(index=index, value=value), self.assertRaises(ValueError):
                config.make_config(*candidate[:7], cache_bytes=candidate[7])

    def test_worker_sandbox_denies_network_and_undeclared_host_files(self):
        script = sandbox_script(Path("/nix/store/tools/bin"), Path("/private/work"))
        self.assertIn("--unshare-net", script)
        self.assertIn("--unshare-pid", script)
        self.assertIn("--die-with-parent", script)
        self.assertIn("--bind /private/work /private/work", script)
        self.assertIn("--ro-bind /nix/store /nix/store", script)
        self.assertIn("--ro-bind /nix/store/tools/bin /bin", script)
        self.assertIn("--ro-bind /nix/store/tools/bin/env /usr/bin/env", script)
        self.assertNotIn("--ro-bind / /", script)
        self.assertNotIn("/home", script)
        self.assertNotIn("/usr/include", script)
        self.assertIn('"$@"', script)
        self.assertIn('--chdir "$(pwd -P)"', script)
        self.assertNotIn('--chdir "$PWD"', script)
        self.assertLess(script.index("--tmpfs /tmp"), script.index("--bind /private/work"))

    def test_json_roundtrip(self):
        self.assertEqual(json.loads(json.dumps(self.config)), self.config)


def unused_port():
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


@unittest.skipUnless(os.environ.get("NATIVELINK_BINARY"), "NATIVELINK_BINARY not set")
class StartupTest(unittest.TestCase):
    def test_startup_and_cleanup(self):
        binary = os.path.abspath(os.environ["NATIVELINK_BINARY"])
        with tempfile.TemporaryDirectory(prefix="nativelink-config-test-") as tmp:
            first, second = unused_port(), unused_port()
            while second == first:
                second = unused_port()
            cfg = config.make_config(tmp, 2, 2_097_152, 1, 1_048_576, first, second, cache_bytes=8 * 1024**2)
            config_path = Path(tmp) / "config.json"
            config_path.write_text(json.dumps(cfg))
            log_path = Path(tmp) / "nativelink.log"
            with log_path.open("w+") as log:
                process = subprocess.Popen([binary, str(config_path)], stdout=log, stderr=subprocess.STDOUT)
                try:
                    deadline = time.monotonic() + 15
                    remaining = {first, second}
                    while remaining and time.monotonic() < deadline and process.poll() is None:
                        for port in list(remaining):
                            try:
                                with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                                    remaining.remove(port)
                            except OSError:
                                pass
                        time.sleep(0.05)
                    log.flush()
                    self.assertIsNone(process.poll(), log_path.read_text())
                    self.assertFalse(remaining, log_path.read_text())
                finally:
                    try:
                        process.terminate()
                    except ProcessLookupError:
                        pass
                    try:
                        process.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)


if __name__ == "__main__":
    unittest.main()
