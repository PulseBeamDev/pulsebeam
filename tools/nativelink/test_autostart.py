"""Fast startup protocol tests; no service or subprocess is actually started."""

import concurrent.futures
from pathlib import Path
import tempfile
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
            [str(self.bazelisk), f"--output_base={self.output}", "build", "//tools/nativelink:server"],
            cwd=self.workspace, check=True)
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


class ProbeTests(unittest.TestCase):
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
