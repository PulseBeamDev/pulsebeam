"""Contract for isolated simulation selection, exceptions and seed replay."""

from contextlib import redirect_stderr, redirect_stdout
import importlib.util
from io import StringIO
import os
from pathlib import Path
import subprocess
import sys
import tempfile
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
            kwargs["stdout"].write(f"complete trace for {name}\n")
            return SimpleNamespace(returncode=int(name in failing))

        with tempfile.TemporaryDirectory() as temp, \
             patch.dict(os.environ, {"TEST_UNDECLARED_OUTPUTS_DIR": temp, "TEST_TMPDIR": temp}), \
             patch.object(RUNNER.sys, "argv", ["runner", "fake-libtest", "--jobs", "1", *args]), \
             patch.object(RUNNER.subprocess, "run", side_effect=run), \
             redirect_stdout(output), redirect_stderr(output):
            try:
                status = RUNNER.main()
            except SystemExit as error:
                status = error.code
            self.traces = [path.read_text() for path in Path(temp).glob("simulations/*.log")]
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
        self.assertIn(f"--run-skipped --seed 4711 --exact --filter {EXCEPTION}", output)
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

    def test_concise_result_and_replay_are_reported_outside_child(self):
        with patch.object(RUNNER.time, "perf_counter", side_effect=[10.0, 12.5]):
            status, executed, output = self.invoke("--filter", FAST, "--seed", "4711")
        self.assertEqual(status, 0)
        self.assertEqual(len(executed), 1)
        self.assertIn("[simulation-run] cases=1 seeds=1 jobs=1", output)
        self.assertIn(f"[simulation-result] PASS {FAST} seed=4711 wall_seconds=2.500", output)
        self.assertIn(f"--seed 4711 --exact --filter {FAST}", output)
        self.assertNotIn("complete trace", output)
        self.assertEqual(self.traces, [])

    def test_complete_failure_trace_is_retained_and_attributable(self):
        status, _, output = self.invoke("--filter", FAST, "--seed", "4711", failing=[FAST])
        self.assertEqual(status, 1)
        self.assertIn("Failure trace:", output)
        self.assertEqual(len(self.traces), 1)
        self.assertIn(f"case={FAST} seed=4711", self.traces[0])
        self.assertIn(f"complete trace for {FAST}", self.traces[0])

    def test_explicit_diagnostics_emit_successful_traces(self):
        status, _, output = self.invoke("--filter", FAST, "--show-output")
        self.assertEqual(status, 0)
        self.assertLess(output.index("complete trace"), output.index("[simulation-result] PASS"))
        self.assertEqual(self.traces, [])

    def test_exact_selection_does_not_silently_match_a_prefix(self):
        status, executed, _ = self.invoke("--filter", "tests::smo", "--exact")
        self.assertEqual(status, 2)
        self.assertEqual(executed, [])
        status, executed, _ = self.invoke("--filter", FAST, "--exact")
        self.assertEqual(status, 0)
        self.assertEqual([item[0] for item in executed], [FAST])

    def test_inherited_replay_seed_is_reported(self):
        with patch.dict(os.environ, {"PULSEBEAM_SIM_SEED": "73"}):
            status, executed, output = self.invoke("--filter", FAST)
        self.assertEqual(status, 0)
        self.assertEqual(executed[0][1], "73")
        self.assertIn("seed=73", output)
        self.assertIn("--seed 73", output)

    def test_invalid_seed_window_fails_before_execution(self):
        status, executed, output = self.invoke("--seed", str(2**64 - 1), "--seeds", "2")
        self.assertEqual(status, 2)
        self.assertEqual(executed, [])
        self.assertIn("unsigned 64-bit integer", output)


# The two cases rendezvous through a FIFO. They cannot complete in one worker,
# and distinct PIDs prove that global clock/RNG hooks never share a process.
LIBTEST = '''import os
from pathlib import Path
import sys

names = ["tests::first", "tests::second"]
if "--list" in sys.argv:
    if "--ignored" not in sys.argv:
        for name in names:
            print(name + ": test")
    sys.exit(0)
assert sys.argv[1:] in [["--exact", name, "--nocapture", "--test-threads=1"] for name in names]
name = sys.argv[2]
if os.environ.get("RENDEZVOUS"):
    if name == names[0]:
        with open(os.environ["RENDEZVOUS"], "w") as pipe:
            pipe.write("ready")
    else:
        with open(os.environ["RENDEZVOUS"]) as pipe:
            assert pipe.read() == "ready"
print("[scoreboard] case=" + name + " pid=" + str(os.getpid()))
print("stderr evidence", file=sys.stderr)
if os.environ.get("FAIL_CASE") == name:
    print("full failure context\\n" * 100)
    sys.exit(7)
'''


class SimulationProcessesContract(unittest.TestCase):
    def run_fixture(self, root, *args, env=None):
        binary = root / "libtest"
        binary.write_text(f"#!{sys.executable}\n" + LIBTEST)
        binary.chmod(0o755)
        return subprocess.run(
            [sys.executable, str(Path(RUNNER.__file__)), str(binary), *args],
            env=os.environ | {"TEST_TMPDIR": str(root), "TEST_UNDECLARED_OUTPUTS_DIR": str(root)} | (env or {}),
            capture_output=True, text=True, timeout=20,
        )

    def test_workers_coexist_in_distinct_processes_with_grouped_metrics(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            fifo = root / "rendezvous"
            os.mkfifo(fifo)
            result = self.run_fixture(root, "--jobs", "2", "--show-output", "--seed", "42",
                                      env={"RENDEZVOUS": str(fifo)})
            self.assertEqual(result.returncode, 0, result.stderr)
            lines = result.stdout.splitlines()
            metrics = [line for line in lines if line.startswith("[scoreboard]")]
            self.assertEqual(len(metrics), 2)
            self.assertNotEqual(metrics[0].split("pid=")[1], metrics[1].split("pid=")[1])
            for metric in metrics:
                name = metric.split("case=")[1].split()[0]
                following = lines[lines.index(metric) + 1:]
                next_result = next(line for line in following if line.startswith("[simulation-result]"))
                self.assertIn(f"PASS {name} seed=42", next_result)
            self.assertEqual(list(root.glob("simulations/*.log")), [])

    def test_failure_keeps_complete_stdout_stderr_and_nonzero_status(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            result = self.run_fixture(root, "--exact", "--filter", "tests::first", "--seed", "73",
                                      env={"FAIL_CASE": "tests::first"})
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertIn("jobs=1", result.stdout)
            self.assertIn("FAIL tests::first seed=73", result.stdout)
            self.assertNotIn("full failure context", result.stdout)
            traces = list(root.glob("simulations/*.log"))
            self.assertEqual(len(traces), 1)
            trace = traces[0].read_text()
            self.assertIn("case=tests::first seed=73", trace)
            self.assertIn("stderr evidence", trace)
            self.assertEqual(trace.count("full failure context"), 100)


if __name__ == "__main__":
    unittest.main()
