"""Fast startup protocol tests; no service or subprocess is actually started."""

import concurrent.futures
import io
import json
from pathlib import Path
import tempfile
import sys
import threading
import unittest
from unittest import mock

import autostart


class StartupTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.state = self.root / "state"
        self.state.mkdir(mode=0o700)
        self.workspace = self.root / "checkout"
        self.output = self.root / "output"
        self.bazelisk = self.root / "bazelisk"
        for name in ("external/+http_archive+nativelink/nativelink", "external/+webrtc+webrtc_host/host-bin"):
            path = self.output / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text("fake")
        self.patches = mock.patch.multiple(
            autostart, identity=mock.DEFAULT, alive=mock.DEFAULT,
            server_locked=mock.DEFAULT, ports_free=mock.DEFAULT,
            process_start=mock.DEFAULT, boot_id=mock.DEFAULT, wait_ready=mock.DEFAULT,
        )
        self.fakes = self.patches.start()
        self.addCleanup(self.patches.stop)
        self.fakes["identity"].return_value = "compatible"
        self.fakes["alive"].return_value = True
        self.fakes["server_locked"].return_value = False
        self.fakes["process_start"].return_value = "123"
        self.fakes["boot_id"].return_value = "boot"
        self.run = mock.patch.object(autostart.subprocess, "run").start()
        self.addCleanup(mock.patch.stopall)
        self.popen = mock.patch.object(autostart.subprocess, "Popen").start()
        self.popen.return_value.pid = 42

    def start(self):
        autostart.start(self.workspace, self.bazelisk, self.output, self.state)

    def marker(self, identity="compatible"):
        return {"pid": 42, "start": "123", "boot": "boot", "identity": identity}

    def test_first_start_and_reuse(self):
        self.start()
        self.run.assert_called_once_with(
            [str(self.bazelisk), f"--output_base={self.output}", "build", "--config=local", "//tools/nativelink:server"],
            cwd=self.workspace, check=True, stdout=sys.stderr)
        args, kwargs = self.popen.call_args
        self.assertEqual(args[0][1:], [str(self.workspace / "tools/nativelink/server.py"),
                         "--binary", str(self.output / "external/+http_archive+nativelink/nativelink"),
                         "--host-bin-file", str(self.output / "external/+webrtc+webrtc_host/host-bin"),
                         "--state", str(self.state)])
        self.assertTrue(kwargs["start_new_session"])
        self.assertEqual(autostart.read_marker(self.state), self.marker())
        self.start()
        self.run.assert_called_once()
        self.popen.assert_called_once()
        self.assertEqual(self.fakes["wait_ready"].call_count, 2)

    def test_stale_and_corrupt_markers_recover(self):
        autostart.write_marker(self.state, self.marker("old"))
        self.fakes["alive"].return_value = False
        self.start()
        self.assertEqual(autostart.read_marker(self.state), self.marker())
        (self.state / "autostart.json").write_text("{broken")
        self.start()
        self.assertEqual(self.run.call_count, 2)

    def test_incompatible_live_service_is_never_killed(self):
        autostart.write_marker(self.state, self.marker("different"))
        with self.assertRaisesRegex(autostart.StartupError, "Incompatible.*stop"):
            self.start()
        self.run.assert_not_called()
        self.popen.assert_not_called()

    def test_unknown_server_or_occupied_port_fails(self):
        self.fakes["server_locked"].return_value = True
        with self.assertRaisesRegex(autostart.StartupError, "unrecognized"):
            self.start()
        self.fakes["server_locked"].return_value = False
        self.fakes["ports_free"].side_effect = autostart.StartupError("Port occupied")
        with self.assertRaisesRegex(autostart.StartupError, "Port occupied"):
            self.start()
        self.run.assert_not_called()

    def test_concurrent_starts_serialize_through_readiness(self):
        entered = threading.Event()
        release = threading.Event()

        def wait(*args):
            entered.set()
            if not release.wait(2):
                raise AssertionError("test did not release readiness")

        self.fakes["wait_ready"].side_effect = wait
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            first = pool.submit(self.start)
            self.assertTrue(entered.wait(2))
            second = pool.submit(self.start)
            # The second caller cannot build or return while readiness holds the lock.
            try:
                with self.assertRaises(concurrent.futures.TimeoutError):
                    second.result(timeout=0.05)
                self.run.assert_called_once()
            finally:
                release.set()
            first.result(timeout=2)
            second.result(timeout=2)
        self.run.assert_called_once()
        self.popen.assert_called_once()

    def test_symlink_lock_is_rejected(self):
        (self.state / "autostart.lock").symlink_to(self.root / "other")
        with self.assertRaises(OSError):
            self.start()
        self.run.assert_not_called()

    def test_failed_startup_cleans_up_its_child(self):
        self.fakes["wait_ready"].side_effect = autostart.StartupError("not ready")
        with self.assertRaisesRegex(autostart.StartupError, "not ready"):
            self.start()
        self.popen.return_value.terminate.assert_called_once()
        self.popen.return_value.wait.assert_called_once()
        self.assertIsNone(autostart.read_marker(self.state))

    def test_inputs_changing_during_build_fail(self):
        self.fakes["identity"].side_effect = ["before", "after"]
        with self.assertRaisesRegex(autostart.StartupError, "inputs changed"):
            self.start()
        self.popen.assert_not_called()


class IsolationTests(StartupTests):
    def setUp(self):
        super().setUp()
        self.env = mock.patch.dict(autostart.os.environ, {"PULSEBEAM_NATIVELINK_ISOLATION": "1"})
        self.env.start()
        self.addCleanup(self.env.stop)
        self.budget = mock.patch.object(autostart, "machine_budget", return_value=(8, 16 * 1024 * 1024))
        self.budget.start()
        self.addCleanup(self.budget.stop)
        self.token = mock.patch.object(autostart.secrets, "token_hex", return_value="unique")
        self.token.start()
        self.addCleanup(self.token.stop)
        self.info = mock.patch.object(autostart, "unit_info").start()
        self.verify = mock.patch.object(autostart, "verify_isolation").start()
        self.info.side_effect = [None, {"Description": "pulsebeam-nativelink-unique", "MainPID": "42"}]
        self.run.return_value.returncode = 0

    # Default-path tests are covered by StartupTests, not inherited under isolation.
    test_first_start_and_reuse = None
    test_stale_and_corrupt_markers_recover = None
    test_incompatible_live_service_is_never_killed = None
    test_unknown_server_or_occupied_port_fails = None
    test_concurrent_starts_serialize_through_readiness = None
    test_symlink_lock_is_rejected = None
    test_failed_startup_cleans_up_its_child = None
    test_inputs_changing_during_build_fail = None

    def test_launch_and_reuse(self):
        self.start()
        command = self.run.call_args_list[1].args[0]
        self.assertIn("--service-type=exec", command)
        self.assertNotIn("--scope", command)
        self.assertIn("--property=CPUQuota=600%", command)
        self.assertIn(f"--property=MemoryMax={12 * 1024**3}", command)
        self.assertEqual(command[-4:], ["--cpu", "6", "--memory-kb", str(10 * 1024 * 1024)])
        marker = autostart.read_marker(self.state)
        self.assertEqual(marker["pid"], 42)
        self.assertEqual(marker["token"], "pulsebeam-nativelink-unique")
        self.assertEqual(self.verify.call_count, 2)
        self.start()
        self.assertEqual(self.verify.call_count, 4)
        self.assertEqual(self.run.call_count, 2)
        self.popen.assert_not_called()

    def test_failure_stops_only_new_matching_unit(self):
        self.info.side_effect = [None, {"Description": "pulsebeam-nativelink-unique", "MainPID": "42"},
                                 {"Description": "pulsebeam-nativelink-unique"}]
        self.fakes["wait_ready"].side_effect = autostart.StartupError("not ready")
        with self.assertRaisesRegex(autostart.StartupError, "not ready"):
            self.start()
        self.assertEqual(self.run.call_args.args[0][:3], ["systemctl", "--user", "stop"])
        self.assertIsNone(autostart.read_marker(self.state))

    def test_unavailable_manager_fails_without_fallback(self):
        self.info.side_effect = autostart.StartupError("user manager unavailable")
        with self.assertRaisesRegex(autostart.StartupError, "user manager"):
            self.start()
        self.run.assert_not_called()
        self.popen.assert_not_called()

    def test_active_previous_unit_is_not_stopped(self):
        (self.state / "isolation-intent.json").write_text(json.dumps({"token": "old"}))
        (self.state / "isolation-intent.json").chmod(0o600)
        self.info.side_effect = [{"ActiveState": "active", "Description": "old"}]
        with self.assertRaisesRegex(autostart.StartupError, "active unit"):
            self.start()
        self.run.assert_not_called()

    def test_unknown_unit_is_not_stopped(self):
        self.info.side_effect = [{"ActiveState": "active", "Description": "foreign"}]
        with self.assertRaisesRegex(autostart.StartupError, "Unrecognized"):
            self.start()
        self.run.assert_not_called()

    def test_launch_failure_does_not_stop_foreign_replacement(self):
        self.info.side_effect = [None, {"Description": "foreign"}]
        self.run.side_effect = [mock.Mock(), mock.Mock(returncode=1, stderr="failed")]
        with self.assertRaisesRegex(autostart.StartupError, "Could not launch"):
            self.start()
        self.assertEqual(self.run.call_count, 2)

    def test_incompatible_marker_is_rejected(self):
        autostart.write_marker(self.state, self.marker())
        with self.assertRaisesRegex(autostart.StartupError, "Incompatible"):
            self.start()
        self.run.assert_not_called()

    def test_reuse_rejects_failed_verification(self):
        self.start()
        self.verify.side_effect = autostart.StartupError("limits changed")
        with self.assertRaisesRegex(autostart.StartupError, "limits changed"):
            self.start()
        self.assertEqual(self.run.call_count, 2)

    def test_recovers_recognized_failed_unit(self):
        (self.state / "isolation-intent.json").write_text(json.dumps({"token": "old"}))
        (self.state / "isolation-intent.json").chmod(0o600)
        self.info.side_effect = [{"ActiveState": "failed", "Description": "old"}, None,
                                 {"Description": "pulsebeam-nativelink-unique", "MainPID": "42"}]
        self.fakes["alive"].return_value = False
        self.start()
        self.assertEqual(self.run.call_args_list[0].args[0][:3], ["systemctl", "--user", "reset-failed"])

    def test_verification_failure_cleans_new_unit(self):
        self.info.side_effect = [None, {"Description": "pulsebeam-nativelink-unique", "MainPID": "42"},
                                 {"Description": "pulsebeam-nativelink-unique"}]
        self.verify.side_effect = autostart.StartupError("limits")
        with self.assertRaisesRegex(autostart.StartupError, "limits"):
            self.start()
        self.assertEqual(self.run.call_args.args[0][:3], ["systemctl", "--user", "stop"])

    def test_budget_and_minimum(self):
        self.assertEqual(autostart.isolation_budget(), (6, 12 * 1024**2, 10 * 1024**2))
        with mock.patch.object(autostart, "machine_budget", return_value=(4, 8 * 1024**2)):
            with self.assertRaisesRegex(autostart.StartupError, "5 GiB"):
                autostart.isolation_budget()
        with mock.patch.object(autostart, "machine_budget", return_value=(4, 16 * 1024**2)):
            self.assertEqual(autostart.isolation_budget()[0], 4)


class IsolationVerificationTests(unittest.TestCase):
    def test_actual_limits_and_membership(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            group = root / "test.service"
            group.mkdir()
            (group / "cpu.max").write_text("600000 100000\n")
            (group / "memory.max").write_text("123904\n")
            info = {"ActiveState": "active", "Description": "token", "MainPID": "42", "ControlGroup": "/test.service"}
            original = Path.read_text
            def read(path, *args, **kwargs):
                if str(path) == "/proc/42/cgroup":
                    return "0::/test.service\n"
                return original(path, *args, **kwargs)
            with mock.patch.object(autostart, "unit_info", return_value=info), \
                 mock.patch.object(autostart, "_cgroup_mounts", return_value=[(root, Path("/"), Path("."))]), \
                 mock.patch.object(Path, "read_text", read):
                autostart.verify_isolation("test.service", "token", 42, 6, 121)
                info["MainPID"] = "43"
                with self.assertRaisesRegex(autostart.StartupError, "expected active"):
                    autostart.verify_isolation("test.service", "token", 42, 6, 121)
                info["MainPID"] = "42"
                (group / "cpu.max").write_text("max 100000\n")
                with self.assertRaisesRegex(autostart.StartupError, "actual limits"):
                    autostart.verify_isolation("test.service", "token", 42, 6, 121)
                (group / "cpu.max").write_text("600000 100000\n")
                (group / "memory.max").write_text("max\n")
                with self.assertRaises(autostart.StartupError):
                    autostart.verify_isolation("test.service", "token", 42, 6, 121)


class ProbeTests(unittest.TestCase):
    def test_cli_reports_tools_from_the_ready_worker(self):
        with tempfile.TemporaryDirectory() as directory:
            state = Path(directory)
            config = {"workers": [{"local": {"platform_properties": {
                "pulsebeam-host-tools": {"values": ["/nix/store/pinned-tools/bin"]},
                "cpu_count": {"values": ["18"]},
            }}}]}
            (state / "config.json").write_text(json.dumps(config))
            with mock.patch.object(autostart, "state_directory", return_value=state), \
                 mock.patch.object(autostart, "start") as start, \
                 mock.patch.object(sys, "argv", ["autostart", "--workspace", "/repo", "--bazelisk", "/bazelisk", "--output-base", "/output", "--print-host-tools"]), \
                 mock.patch.object(sys, "stdout", new_callable=io.StringIO) as output:
                self.assertEqual(autostart.main(), 0)
                self.assertEqual(output.getvalue(), "/nix/store/pinned-tools/bin\n36\n")
                start.assert_called_once()

    def test_readiness_requires_both_owned_listeners_and_lock(self):
        with mock.patch.multiple(autostart, alive=mock.DEFAULT, server_locked=mock.DEFAULT,
                                 owns_lock=mock.DEFAULT, listening_ports=mock.DEFAULT,
                                 time=mock.DEFAULT) as f:
            f["alive"].return_value = True
            f["server_locked"].return_value = True
            f["owns_lock"].return_value = True
            f["listening_ports"].side_effect = [{50051}, {50051, 50061}]
            f["time"].monotonic.return_value = 0
            autostart.wait_ready(Path("/state"), {"pid": 7})
            self.assertEqual(f["time"].sleep.call_count, 1)

    def test_listener_probe_only_counts_process_socket_inodes(self):
        fd = mock.Mock()
        fd_dir = mock.Mock()
        fd_dir.iterdir.return_value = [fd]
        tcp = mock.Mock()
        tcp.read_text.return_value = ("header\n"
            "0: 0100007F:C387 00000000:0000 0A 0:0 0:0 0 1000 0 111\n"
            "1: 0100007F:C38F 00000000:0000 0A 0:0 0:0 0 1000 0 222\n")
        with mock.patch.object(autostart, "Path", side_effect=[fd_dir, tcp]), \
             mock.patch.object(autostart.os, "readlink", return_value="socket:[111]"):
            self.assertEqual(autostart.listening_ports(7), {50055})

    def test_readiness_times_out_for_unowned_listeners(self):
        with mock.patch.multiple(autostart, alive=mock.DEFAULT, server_locked=mock.DEFAULT,
                                 owns_lock=mock.DEFAULT, listening_ports=mock.DEFAULT,
                                 time=mock.DEFAULT) as f:
            f["alive"].return_value = True
            f["server_locked"].return_value = True
            f["owns_lock"].return_value = False
            f["listening_ports"].return_value = {50051, 50061}
            f["time"].monotonic.side_effect = [0, 1, 31]
            with self.assertRaisesRegex(autostart.StartupError, "did not become ready"):
                autostart.wait_ready(Path("/state"), {"pid": 7})
            f["listening_ports"].assert_not_called()

    def test_occupied_port_is_not_treated_as_service(self):
        sock = mock.Mock()
        sock.bind.side_effect = OSError("in use")
        with mock.patch.object(autostart.socket, "socket", return_value=sock):
            with self.assertRaisesRegex(autostart.StartupError, "50051.*occupied"):
                autostart.ports_free()
        sock.close.assert_called_once()

    def test_identity_tracks_actual_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in autostart.INPUTS:
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(name)
            module = root / "MODULE.bazel"
            module.write_text('http_archive(\n    name = "nativelink",\n    sha256 = "one",\n)\n')
            before = autostart.identity(root)
            module.write_text(module.read_text() + '\nbazel_dep(name = "unrelated", version = "2")\n')
            self.assertEqual(before, autostart.identity(root))
            (root / "tools/nativelink/config.py").write_text("changed")
            self.assertNotEqual(before, autostart.identity(root))

    def test_stale_pid_and_boot_identity(self):
        with mock.patch.object(autostart, "boot_id", return_value="current"), \
             mock.patch.object(autostart, "process_start", return_value="123"):
            self.assertFalse(autostart.alive({"boot": "old", "pid": 1, "start": "123"}))
            self.assertFalse(autostart.alive({"boot": "current", "pid": 1, "start": "456"}))
            self.assertTrue(autostart.alive({"boot": "current", "pid": 1, "start": "123"}))


if __name__ == "__main__":
    unittest.main()
