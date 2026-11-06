import tempfile
import unittest
from pathlib import Path
from unittest import mock

import resources


class ResourceTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.mount = self.base / "cgroup mount"
        self.mount.mkdir()
        (self.mount / "cgroup.controllers").write_text("cpu memory\n")
        self.cgroup = self.base / "proc-cgroup"
        self.mountinfo = self.base / "mountinfo"
        self.meminfo = self.base / "meminfo"
        self.meminfo.write_text("MemTotal:  32000000 kB\n")
        for name, value in (("_PROC_CGROUP", self.cgroup),
                            ("_PROC_MOUNTINFO", self.mountinfo),
                            ("_PROC_MEMINFO", self.meminfo)):
            patch = mock.patch.object(resources, name, value)
            patch.start()
            self.addCleanup(patch.stop)
        affinity = mock.patch.object(resources.os, "sched_getaffinity", return_value=set(range(20)))
        affinity.start()
        self.addCleanup(affinity.stop)

    def hierarchy(self, membership="/parent/child", root="/", mount=None):
        mount = mount or self.mount
        self.cgroup.write_text(f"0::{membership}\n")
        escaped = str(mount).replace("\\", "\\134").replace(" ", "\\040")
        self.mountinfo.write_text(f"42 1 0:28 {root} {escaped} rw - cgroup2 cgroup rw\n")

    def limits(self, relative, cpu="max 100000", memory="max"):
        directory = self.mount / relative
        directory.mkdir(parents=True, exist_ok=True)
        (directory / "cgroup.controllers").write_text("cpu memory\n")
        (directory / "cpu.max").write_text(cpu + "\n")
        (directory / "memory.max").write_text(memory + "\n")

    def test_nested_ancestors_cap_affinity_and_host_memory(self):
        self.hierarchy()
        self.limits("parent", "250000 100000", "8192000")
        self.limits("parent/child", "400000 100000", "16384000")
        self.assertEqual(resources.machine_budget(), (2, 8000))

    def test_fractional_quota_has_minimum_one_and_memory_is_rounded_down(self):
        self.hierarchy()
        self.limits("parent", "max 100000", "max")
        self.limits("parent/child", "50000 100000", "2049")
        self.assertEqual(resources.machine_budget(), (1, 2))

    def test_unlimited_limits_use_affinity_and_host_memory(self):
        self.hierarchy()
        self.limits("parent")
        self.limits("parent/child")
        self.assertEqual(resources.machine_budget(), (20, 32000000))

    def test_limits_above_host_capacity_do_not_increase_budget(self):
        self.hierarchy()
        self.limits("parent", "5000000 100000", "999999999999")
        self.limits("parent/child")
        self.assertEqual(resources.machine_budget(), (20, 32000000))

    def test_mount_root_maps_membership_to_nonstandard_mount(self):
        self.hierarchy("/parent/child", "/parent")
        self.limits(".", "300000 100000", "4096000")
        self.limits("child", "max 100000", "max")
        self.assertEqual(resources.machine_budget(), (3, 4000))

    def test_all_matching_mounts_are_considered(self):
        self.hierarchy("/parent/child", "/parent")
        self.limits(".", "500000 100000", "max")
        self.limits("child")
        other = self.base / "other"
        (other / "parent" / "child").mkdir(parents=True)
        for directory in (other / "parent", other / "parent" / "child"):
            (directory / "cgroup.controllers").write_text("cpu memory\n")
            (directory / "cpu.max").write_text("max 100000")
            (directory / "memory.max").write_text("max")
        (other / "parent" / "memory.max").write_text("1024000")
        (other / "cgroup.controllers").write_text("cpu memory\n")
        with self.mountinfo.open("a") as stream:
            stream.write(f"43 1 0:28 / {other} rw - cgroup2 cgroup rw\n")
        self.assertEqual(resources.machine_budget(), (5, 1000))

    def test_missing_v2_falls_back_without_reading_mountinfo(self):
        self.cgroup.write_text("2:cpu,cpuacct:/legacy\n")
        self.assertEqual(resources.machine_budget(), (20, 32000000))

    def test_malformed_limits_fail_explicitly(self):
        for cpu, memory in (("broken", "max"), ("max 0", "max"),
                            ("0 100000", "max"), ("1 nope", "max"),
                            ("max 100000", "-1"), ("max 100000", "max extra")):
            with self.subTest(cpu=cpu, memory=memory):
                self.hierarchy()
                self.limits("parent")
                self.limits("parent/child", cpu, memory)
                with self.assertRaisesRegex(ValueError, "invalid cgroup limit"):
                    resources.machine_budget()

    def test_disabled_controllers_do_not_hide_ancestor_limits(self):
        self.hierarchy()
        self.limits("parent", "300000 100000", "8192000")
        self.limits("parent/child")
        child = self.mount / "parent/child"
        (child / "cgroup.controllers").write_text("io\n")
        (child / "cpu.max").unlink()
        (child / "memory.max").unlink()
        self.assertEqual(resources.machine_budget(), (3, 8000))

    def test_sub_kib_memory_limit_fails_with_clear_error(self):
        self.hierarchy()
        self.limits("parent")
        self.limits("parent/child", memory="1023")
        with self.assertRaisesRegex(ValueError, "less than 1 KiB"):
            resources.machine_budget()

    def test_missing_non_root_limit_is_an_error(self):
        self.hierarchy()
        self.limits("parent")
        self.limits("parent/child")
        (self.mount / "parent" / "memory.max").unlink()
        with self.assertRaisesRegex(ValueError, "missing cgroup limit.*memory.max"):
            resources.machine_budget()

    def test_missing_mount_is_not_treated_as_unlimited_root(self):
        self.hierarchy("/")
        (self.mount / "cgroup.controllers").unlink()
        with self.assertRaisesRegex(ValueError, "cannot read cgroup directory"):
            resources.machine_budget()

    def test_inaccessible_limit_is_an_error(self):
        self.hierarchy()
        self.limits("parent")
        self.limits("parent/child")
        original = Path.read_text
        def read(path, *args, **kwargs):
            if path == self.mount / "parent" / "cpu.max":
                raise PermissionError("denied")
            return original(path, *args, **kwargs)
        with mock.patch.object(Path, "read_text", read):
            with self.assertRaisesRegex(ValueError, "cannot read cgroup limit.*cpu.max"):
                resources.machine_budget()

    def test_unmapped_membership_is_an_error(self):
        self.hierarchy("/different/child", "/parent")
        with self.assertRaisesRegex(ValueError, "cannot locate cgroup v2"):
            resources.machine_budget()

    def test_malformed_mountinfo_is_an_error(self):
        self.hierarchy()
        self.mountinfo.write_text("not mountinfo\n")
        with self.assertRaisesRegex(ValueError, "invalid mountinfo"):
            resources.machine_budget()

    def test_invalid_membership_path_is_an_error(self):
        self.hierarchy("/parent/../child")
        with self.assertRaisesRegex(ValueError, "invalid cgroup path"):
            resources.machine_budget()

    def test_client_dispatch_window_scales_with_scheduler_capacity(self):
        self.assertEqual(resources.client_jobs(20), 40)
        self.assertEqual(resources.client_jobs(1), 8)
        self.assertEqual(resources.client_jobs(100), 128)

    def test_missing_host_memory_is_an_error(self):
        self.meminfo.write_text("MemFree: 1 kB\n")
        with self.assertRaisesRegex(ValueError, "host memory"):
            resources.machine_budget()


if __name__ == "__main__":
    unittest.main()
