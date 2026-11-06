import unittest
from unittest import mock

import resources


class ResourceTests(unittest.TestCase):
    def test_machine_budget_uses_all_cpus_and_leaves_memory_headroom(self):
        for cpus, memory in [(1, 4_000_000), (20, 32_000_000), (160, 256_000_000)]:
            with self.subTest(cpus=cpus, memory=memory), \
                 mock.patch.object(resources.os, "sched_getaffinity", return_value=set(range(cpus))), \
                 mock.patch.object(resources.Path, "read_text", return_value=f"MemTotal: {memory} kB\n"):
                self.assertEqual(resources.machine_budget(), (cpus, memory * 3 // 4))

    def test_cache_budget_scales_with_state_filesystem_capacity(self):
        for blocks, block_size in [(1000, 4096), (1_000_000, 8192)]:
            volume = mock.Mock(f_blocks=blocks, f_frsize=block_size)
            with self.subTest(blocks=blocks, block_size=block_size), \
                 mock.patch.object(resources.os, "statvfs", return_value=volume) as statvfs:
                self.assertEqual(resources.cache_budget("/state"), blocks * block_size // 10)
                statvfs.assert_called_once_with("/state")

    def test_unknown_filesystem_capacity_is_an_error(self):
        with mock.patch.object(resources.os, "statvfs", return_value=mock.Mock(f_blocks=0, f_frsize=4096)):
            with self.assertRaisesRegex(ValueError, "filesystem capacity"):
                resources.cache_budget("/state")

    def test_client_dispatch_window_scales_with_scheduler_capacity(self):
        self.assertEqual(resources.client_jobs(20), 40)
        self.assertEqual(resources.client_jobs(1), 2)
        self.assertEqual(resources.client_jobs(100), 200)
        with self.assertRaises(ValueError):
            resources.client_jobs(0)

    def test_missing_host_memory_is_an_error(self):
        with mock.patch.object(resources.Path, "read_text", return_value="MemFree: 1 kB\n"):
            with self.assertRaisesRegex(ValueError, "host memory"):
                resources.machine_budget()


if __name__ == "__main__":
    unittest.main()
