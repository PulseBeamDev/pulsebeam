import unittest
from unittest import mock

import resources


class ResourceTests(unittest.TestCase):
    def test_machine_budget_uses_available_cpus_and_total_memory(self):
        with mock.patch.object(resources.os, "sched_getaffinity", return_value=set(range(20))), \
             mock.patch.object(resources.Path, "read_text", return_value="MemTotal:  32000000 kB\n"):
            self.assertEqual(resources.machine_budget(), (20, 32000000))

    def test_client_dispatch_window_scales_with_scheduler_capacity(self):
        self.assertEqual(resources.client_jobs(20), 40)
        self.assertEqual(resources.client_jobs(1), 8)
        self.assertEqual(resources.client_jobs(100), 128)

    def test_missing_host_memory_is_an_error(self):
        with mock.patch.object(resources.Path, "read_text", return_value="MemFree: 1 kB\n"):
            with self.assertRaisesRegex(ValueError, "host memory"):
                resources.machine_budget()


if __name__ == "__main__":
    unittest.main()
