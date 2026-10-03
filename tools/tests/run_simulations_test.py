"""Contract for isolated simulation selection, exceptions and seed replay."""

from contextlib import redirect_stderr, redirect_stdout
import importlib.util
from io import StringIO
from pathlib import Path
from types import SimpleNamespace
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location(
    "run_simulations", Path(__file__).resolve().parents[1] / "run-simulations.py"
)
RUNNER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RUNNER)

EXCEPTION = "tests::native_runtime::native_agents_prove_media_topics_reconnect_and_close"
FAST = "tests::smoke"
SLOW = "tests::slow::pressure"
IGNORED = "tests::exploration"


class SimulationRunnerContract(unittest.TestCase):
    def invoke(self, *args, failing=()):
        executed = []
        output = StringIO()

        def run(command, **kwargs):
            if "--list" in command:
                names = [IGNORED] if "--ignored" in command else [EXCEPTION, FAST, SLOW, IGNORED]
                return SimpleNamespace(stdout="".join(f"{name}: test\n" for name in names))
            name = command[2]
            executed.append((name, kwargs["env"].get("PULSEBEAM_SIM_SEED"), command))
            return SimpleNamespace(returncode=int(name in failing))

        with patch.object(RUNNER.sys, "argv", ["runner", "fake-libtest", "--jobs", "1", *args]), \
             patch.object(RUNNER.subprocess, "run", side_effect=run), \
             redirect_stdout(output), redirect_stderr(output):
            try:
                status = RUNNER.main()
            except SystemExit as error:
                status = error.code
        return status, executed, output.getvalue()

    def test_exact_exception_does_not_exclude_other_fast_cases(self):
        status, executed, output = self.invoke("--skip", EXCEPTION)
        self.assertEqual(status, 0)
        self.assertEqual([item[0] for item in executed], [FAST])
        self.assertIn(f"SKIP {EXCEPTION}", output)
        self.assertIn("native-dtls-exception.md", output)
        self.assertEqual(executed[0][2][1:], ["--exact", FAST, "--nocapture", "--test-threads=1"])

    def test_explicit_opt_in_runs_exception_and_propagates_failure(self):
        status, executed, output = self.invoke(
            "--skip", EXCEPTION, "--run-skipped", "--filter", EXCEPTION,
            "--seed", "4711", failing=[EXCEPTION],
        )
        self.assertEqual(status, 1)
        self.assertEqual([(item[0], item[1]) for item in executed], [(EXCEPTION, "4711")])
        self.assertIn(f"FAIL {EXCEPTION}", output)
        self.assertIn(f"--run-skipped --seed 4711 --filter {EXCEPTION}", output)
        self.assertNotIn("SKIP ", output)

    def test_skipped_only_selection_is_not_a_success(self):
        status, executed, output = self.invoke("--skip", EXCEPTION, "--filter", EXCEPTION)
        self.assertEqual(status, 2)
        self.assertEqual(executed, [])
        self.assertIn("no non-ignored simulations", output)

    def test_slow_selection_and_seed_window_remain_intact(self):
        status, executed, _ = self.invoke("--skip", EXCEPTION, "--mode", "slow", "--seed", "0", "--seeds", "2")
        self.assertEqual(status, 0)
        self.assertEqual([(item[0], item[1]) for item in executed], [(SLOW, "0"), (SLOW, "1")])

    def test_without_exception_configuration_native_case_is_selected(self):
        status, executed, output = self.invoke()
        self.assertEqual(status, 0)
        self.assertEqual({item[0] for item in executed}, {FAST, EXCEPTION})
        self.assertNotIn("SKIP ", output)

    def test_wall_timing_and_concurrency_are_reported_outside_child(self):
        with patch.object(RUNNER.time, "perf_counter", side_effect=[10.0, 12.5]):
            status, executed, output = self.invoke("--filter", FAST, "--seed", "4711")
        self.assertEqual(status, 0)
        self.assertEqual(len(executed), 1)
        self.assertIn("[simulation-run] cases=1 seeds=1 jobs=1 cpus=", output)
        self.assertIn(f"[simulation-timing] START {FAST} seed=4711", output)
        self.assertIn(f"[simulation-timing] END {FAST} wall_seconds=2.500", output)
        self.assertIn(f"[simulation-result] PASS {FAST}", output)

    def test_invalid_seed_window_fails_before_execution(self):
        status, executed, output = self.invoke("--seed", str(2**64 - 1), "--seeds", "2")
        self.assertEqual(status, 2)
        self.assertEqual(executed, [])
        self.assertIn("unsigned 64-bit integer", output)


if __name__ == "__main__":
    unittest.main()
