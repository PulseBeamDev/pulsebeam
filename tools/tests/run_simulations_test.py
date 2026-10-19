"""Contract for isolated simulation selection, exceptions and seed replay."""

from contextlib import redirect_stderr, redirect_stdout
import argparse
import importlib.util
import json
from io import StringIO
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
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
PROVENANCE = "STABLE_SIM_REVISION " + "a" * 40 + "\nSTABLE_SIM_DIRTY true\nSTABLE_SIM_FINGERPRINT " + "b" * 64 + "\n"


class SimulationRunnerContract(unittest.TestCase):
    def invoke(self, *args, failing=()):
        executed = []
        output = StringIO()

        def run(command, trace, **kwargs):
            if "--list" in command:
                names = [IGNORED] if "--ignored" in command else [EXCEPTION, FAST, SLOW, IGNORED]
                trace.write("".join(f"{name}: test\n" for name in names))
                return 0, None
            name = command[2]
            executed.append((name, kwargs["env"].get("PULSEBEAM_SIM_SEED"), command))
            trace.write(f"complete trace for {name}\n")
            trace.write("[simulation-generated] budget=20 accepted=28 rejected=0 failed=0\n")
            return int(name in failing), "exit" if name in failing else None

        with tempfile.TemporaryDirectory() as temp, \
             patch.dict(os.environ, {"TEST_UNDECLARED_OUTPUTS_DIR": temp, "TEST_TMPDIR": temp}), \
             patch.object(RUNNER.sys, "argv", ["runner", "fake-libtest", "--candidate", str(Path(temp) / "candidate"), "--jobs", "1", *args]), \
             patch.object(RUNNER, "run_process", side_effect=run), \
             redirect_stdout(output), redirect_stderr(output):
            (Path(temp) / "candidate").write_text(PROVENANCE)
            try:
                status = RUNNER.main()
            except SystemExit as error:
                status = error.code
            self.traces = [path.read_text() for path in Path(temp).rglob("*.log")]
            manifests = list(Path(temp).rglob("manifest.json"))
            self.manifest = json.loads(manifests[0].read_text()) if manifests else None
            self.replays = [json.loads(path.read_text()) for path in Path(temp).glob("simulations/run-*/case-*/replay.json")]
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
        self.assertTrue(self.replays[0]["selection"]["run_skipped"])
        self.assertEqual(self.replays[0]["selection"]["from_seed"], 4711)
        self.assertIn("--replay-bundle", output)
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
        self.assertTrue(all(item["seed_selection"] == "authored-default" and item["seed"] is None
                            for item in self.manifest["cases"]))

    def test_concise_result_and_replay_are_reported_outside_child(self):
        with patch.object(RUNNER.time, "perf_counter", side_effect=[10.0, 12.5]):
            status, executed, output = self.invoke("--filter", FAST, "--seed", "4711")
        self.assertEqual(status, 0)
        self.assertEqual(len(executed), 1)
        self.assertIn("[simulation-run] cases=1 seeds=1 jobs=1", output)
        self.assertIn(f"[simulation-result] PASS {FAST} seed=4711 wall_seconds=2.500", output)
        self.assertEqual(self.replays[0]["selection"]["from_seed"], 4711)
        self.assertEqual(self.replays[0]["selection"]["filter"], FAST)
        self.assertIn("--replay-bundle", output)
        self.assertNotIn("complete trace", output)
        self.assertIn("[simulation-generated] budget=20 accepted=28 rejected=0 failed=0", output)
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
        self.assertEqual(self.replays[0]["selection"]["from_seed"], 73)

    def test_invalid_host_deadlines_fail_before_execution(self):
        for option in ["--discovery-timeout", "--case-timeout"]:
            for value in ["0", "-1", "nan", "inf"]:
                with self.subTest(option=option, value=value):
                    status, executed, output = self.invoke(option, value)
                    self.assertEqual(status, 2)
                    self.assertEqual(executed, [])
                    self.assertIn("host deadlines must be finite and positive", output)

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
import signal
import subprocess

names = ["tests::first", "tests::second"]
blocked = os.environ.get("BLOCK_PROCESS")
if blocked == ("discovery" if "--list" in sys.argv else "case"):
    # The descendant acknowledges startup before the parent blocks, so cleanup
    # is checked on a process that existed, not an unobserved attempted spawn.
    descendant = subprocess.Popen([sys.executable, "-c",
        "import os,signal; print(os.getpid(), flush=True); signal.pause()"],
        stdout=subprocess.PIPE, text=True)
    Path(os.environ["DESCENDANT_PID"]).write_text(descendant.stdout.readline())
    print("blocked " + blocked, flush=True)
    signal.pause()

if "--list" in sys.argv:
    if os.environ.get("MUTATE_CORPUS"):
        Path(os.environ["MUTATE_CORPUS"]).write_text("changed after capture")
    if os.environ.get("FAIL_DISCOVERY"):
        print("discovery failure context", file=sys.stderr)
        sys.exit(9)
    if "--ignored" not in sys.argv:
        for name in names:
            print(name + ": test")
        if os.environ.get("REMOVE_AFTER_DISCOVERY"):
            Path(sys.argv[0]).unlink()
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
if os.environ.get("EXPECT_CORPUS"):
    corpus = Path(os.environ["PULSEBEAM_SIM_REGRESSIONS"]).read_text()
    assert corpus == os.environ["EXPECT_CORPUS"], corpus
    print("captured corpus=" + corpus)
    outputs = Path(os.environ["TEST_UNDECLARED_OUTPUTS_DIR"])
    (outputs / "family.regressions").write_text(corpus + "new minimized state")
    (outputs / "scenario.json").write_text('{"scenario": "concrete"}')
    (Path(os.environ["TEST_TMPDIR"]) / "nested.regressions").write_text(corpus)
    assert os.environ["RUST_LOG"] == "off"
    assert os.environ["PROPTEST_CASES"] == "3"
if os.environ.get("FAIL_CASE") == name:
    print("full failure context\\n" * 100)
    sys.exit(7)
'''


class SimulationProcessesContract(unittest.TestCase):
    def run_fixture(self, root, *args, env=None, standalone=False):
        binary = root / "libtest"
        if not binary.exists():
            binary.write_text(f"#!{sys.executable}\n" + LIBTEST)
            binary.chmod(0o755)
        provenance = root / "candidate"
        provenance.write_text(PROVENANCE)
        environment = os.environ | {"TEST_TMPDIR": str(root), "TEST_UNDECLARED_OUTPUTS_DIR": str(root)} | (env or {})
        if standalone:
            environment.pop("TEST_UNDECLARED_OUTPUTS_DIR", None)
            environment.pop("TEST_TMPDIR", None)
        return subprocess.run(
            [sys.executable, str(Path(RUNNER.__file__)), str(binary), "--candidate", str(provenance), *args],
            env=environment,
            capture_output=True, text=True, timeout=20,
        )

    def test_host_deadlines_fail_and_terminate_descendants(self):
        for blocked, option in [("discovery", "--discovery-timeout"), ("case", "--case-timeout")]:
            with self.subTest(blocked=blocked), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                pid_file = root / "descendant.pid"
                result = self.run_fixture(
                    root, "--exact", "--filter", "tests::first", "--seed", "73", option, "1",
                    env={"BLOCK_PROCESS": blocked, "DESCENDANT_PID": str(pid_file)},
                )
                self.assertEqual(result.returncode, 1, result.stderr)
                output = result.stdout + result.stderr
                self.assertIn("FAIL", output)
                self.assertIn("seed=73", output)
                self.assertIn("Failure trace:", output)
                traces = list(root.glob("simulations/run-*/**/*.log"))
                self.assertEqual(len(traces), 1)
                trace = traces[0].read_text()
                self.assertIn("command=", trace)
                self.assertIn("seed=73", trace)
                self.assertIn("blocked " + blocked, trace)
                self.assertIn("TIMEOUT after 1 host seconds; process group terminated", trace)
                pid = int(pid_file.read_text())
                status = Path(f"/proc/{pid}/status")
                deadline = time.monotonic() + 2
                while status.exists():
                    try:
                        state = next(line for line in status.read_text().splitlines()
                                     if line.startswith("State:")).split()[1]
                    except FileNotFoundError:
                        break
                    # An orphan may await reaping by the host init. A zombie
                    # has exited and cannot retain resources or run helpers.
                    if state in ["Z", "X"]:
                        break
                    if time.monotonic() >= deadline:
                        self.fail("descendant survived the host deadline")
                    time.sleep(0.01)
                if blocked == "case":
                    self.assertIn("--replay-bundle", output)
                    record = json.loads(next(root.glob("simulations/run-*/case-*/result.json")).read_text())
                    self.assertEqual(record["failure"], "timeout")

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
            self.assertEqual(list(root.glob("simulations/run-*/**/*.log")), [])

    def test_failure_keeps_complete_stdout_stderr_and_nonzero_status(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            result = self.run_fixture(root, "--exact", "--filter", "tests::first", "--seed", "73",
                                      env={"FAIL_CASE": "tests::first"})
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertIn("jobs=1", result.stdout)
            self.assertIn("FAIL tests::first seed=73", result.stdout)
            self.assertNotIn("full failure context", result.stdout)
            traces = list(root.glob("simulations/run-*/**/*.log"))
            self.assertEqual(len(traces), 1)
            trace = traces[0].read_text()
            self.assertIn("case=tests::first seed=73", trace)
            self.assertIn("stderr evidence", trace)
            self.assertEqual(trace.count("full failure context"), 100)


    def test_portable_bundle_replays_captured_inputs_settings_and_emitted_artifacts(self):
        import shutil

        with tempfile.TemporaryDirectory() as destination:
            fresh = Path(destination)
            with tempfile.TemporaryDirectory() as original:
                root = Path(original)
                corpus = root / "external.regressions"
                corpus.write_text("retained RNG input\n")
                env = {"PULSEBEAM_SIM_REGRESSIONS": str(corpus), "EXPECT_CORPUS": corpus.read_text(),
                       "RUST_LOG": "off", "PROPTEST_CASES": "3", "PRIVATE_TOKEN": "never retain this",
                       "FAIL_CASE": "tests::first", "MUTATE_CORPUS": str(corpus)}
                result = self.run_fixture(root, "--filter", "tests::first", "--exact", "--seed", "73", env=env)
                self.assertEqual(result.returncode, 1, result.stderr)
                bundle = next(root.glob("simulations/run-*"))
                manifest = json.loads((bundle / "manifest.json").read_text())
                self.assertEqual(manifest["candidate"]["dirty"], True)
                self.assertEqual(manifest["selection"]["from_seed"], 73)
                self.assertEqual(manifest["settings"], {"RUST_LOG": "off", "PROPTEST_CASES": "3"})
                self.assertEqual(corpus.read_text(), "changed after capture")
                case = bundle / "case-0000"
                self.assertEqual((case / "outputs/family.regressions").read_text(), env["EXPECT_CORPUS"] + "new minimized state")
                self.assertIn("outputs/scenario.json", manifest["cases"][0]["artifacts"])
                self.assertIn("outputs/scratch/nested.regressions", manifest["cases"][0]["artifacts"])
                self.assertNotIn("never retain this", (bundle / "manifest.json").read_text())
                shutil.move(str(bundle), fresh / "portable")
            # The original temporary tree, supplied corpus and executable no
            # longer exist. A fresh process receives only the transported bundle.
            replay = self.run_fixture(fresh, "--replay-bundle", str(fresh / "portable/case-0000"),
                                      env={"EXPECT_CORPUS": "retained RNG input\n", "RUST_LOG": "wrong",
                                           "PROPTEST_CASES": "999", "FAIL_CASE": "tests::first"})
            self.assertEqual(replay.returncode, 1, replay.stderr)
            new = next(fresh.glob("simulations/run-*"))
            record = json.loads((new / "manifest.json").read_text())["cases"][0]
            self.assertEqual(record["seed"], 73)
            self.assertEqual(record["exit_status"], 7)
            self.assertIn("captured corpus=retained RNG input", (new / "case-0000/trace.log").read_text())
            wrong = fresh / "other-candidate"
            wrong.write_text(PROVENANCE.replace("b" * 64, "c" * 64))
            mismatch = self.run_fixture(fresh, "--replay-bundle", str(fresh / "portable"), "--candidate", str(wrong))
            self.assertEqual(mismatch.returncode, 1)
            self.assertIn("recorded actual source candidate", mismatch.stderr)
            (fresh / "portable/inputs/supplied.regressions").write_text("tampered")
            mismatch = self.run_fixture(fresh, "--replay-bundle", str(fresh / "portable"))
            self.assertEqual(mismatch.returncode, 1)
            self.assertIn("checksum mismatch", mismatch.stderr)

    def test_provenance_input_and_evidence_errors_are_infrastructure_failures(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            bad_candidate = root / "bad-candidate"
            for contents in [None, "STABLE_SIM_REVISION " + "a" * 40 + "\n"]:
                if contents is not None:
                    bad_candidate.write_text(contents)
                result = self.run_fixture(root, "--candidate", str(bad_candidate))
                self.assertEqual(result.returncode, 1)
                manifest = json.loads(max(root.glob("simulations/run-*/manifest.json"), key=lambda path: path.stat().st_mtime_ns).read_text())
                self.assertEqual(manifest["status"], "infrastructure")
                self.assertEqual(manifest["candidate"], None)
                self.assertEqual(manifest["discovery"], [])
            result = self.run_fixture(root, env={"PULSEBEAM_SIM_REGRESSIONS": str(root / "absent")})
            self.assertEqual(result.returncode, 1)
            self.assertIn("simulation-infrastructure", result.stderr)
            (root / "not-directory").write_text("file")
            result = self.run_fixture(root, env={"TEST_UNDECLARED_OUTPUTS_DIR": str(root / "not-directory")})
            self.assertEqual(result.returncode, 1)
            self.assertIn("unable to create required evidence root", result.stderr)

    def test_discovery_and_launch_errors_keep_diagnostics(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            result = self.run_fixture(root, env={"FAIL_DISCOVERY": "1"})
            self.assertEqual(result.returncode, 1)
            bundle = next(root.glob("simulations/run-*"))
            self.assertEqual(json.loads((bundle / "manifest.json").read_text())["failure"], "discovery-exit")
            self.assertIn("discovery failure context", (bundle / "discovery-ignored.log").read_text())
            # Break the executable after preparing the same fixture.
            (root / "libtest").chmod(0o644)
            result = self.run_fixture(root)
            self.assertEqual(result.returncode, 1)
            manifests = [json.loads(path.read_text()) for path in root.glob("simulations/run-*/manifest.json")]
            self.assertIn("discovery-launch", [item.get("failure") for item in manifests])
            self.assertIn("Unable to start child", next(path for path in root.glob("simulations/run-*/discovery-ignored.log") if "Unable to start" in path.read_text()).read_text())
            (root / "libtest").chmod(0o755)
            result = self.run_fixture(root, "--exact", "--filter", "tests::first", env={"REMOVE_AFTER_DISCOVERY": "1"})
            self.assertEqual(result.returncode, 1)
            record = json.loads(next(root.glob("simulations/run-*/case-*/result.json")).read_text())
            self.assertEqual(record["failure"], "launch")
            self.assertIn("Unable to start child", next(root.glob("simulations/run-*/case-*/trace.log")).read_text())

    def test_explicit_standalone_destination_is_a_unique_owned_parent(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            parent = root / "retained"
            result = self.run_fixture(root, "--jobs", "1", "--artifacts-dir", str(parent), standalone=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            bundle = next(parent.glob("run-*"))
            self.assertIn(str(bundle), result.stdout)
            self.assertEqual(json.loads((bundle / "manifest.json").read_text())["status"], "passed")
            self.assertEqual(list(bundle.rglob("*.log")), [])

    def test_overlapping_invocations_have_disjoint_evidence(self):
        from concurrent.futures import ThreadPoolExecutor

        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.run_fixture(root)
            with ThreadPoolExecutor(max_workers=2) as executor:
                results = list(executor.map(lambda seed: self.run_fixture(root, "--seed", str(seed),
                                      env={"FAIL_CASE": "tests::first"}), [11, 22]))
            self.assertEqual([result.returncode for result in results], [1, 1])
            manifests = [json.loads(path.read_text()) for path in root.glob("simulations/run-*/manifest.json")]
            self.assertEqual(len(manifests), 3)
            failures = [item for item in manifests if item["status"] == "failed"]
            self.assertEqual({item["cases"][0]["seed"] for item in failures}, {11, 22})
            for trace in root.glob("simulations/run-*/case-*/trace.log"):
                self.assertEqual(trace.read_text().count("full failure context"), 100)


class SimulationProvenanceContract(unittest.TestCase):
    def test_declared_status_is_available_without_repository_access(self):
        identity = RUNNER.candidate(OPTIONS.candidate)
        self.assertEqual(len(identity["fingerprint"]), 64)
        self.assertIsInstance(identity["dirty"], bool)

    def test_workspace_status_attributes_clean_dirty_staged_and_untracked_sources(self):
        script = Path(RUNNER.__file__).with_name("simulation-status.sh")
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)

            def git(*args):
                return subprocess.run(["git", *args], cwd=root, capture_output=True, text=True, check=True)

            def status():
                result = subprocess.run(["bash", str(script)], cwd=root, capture_output=True, text=True, check=True)
                path = root / "metadata"
                path.write_text(result.stdout)
                return RUNNER.candidate(path)

            git("init", "-q")
            git("config", "user.email", "fixture@example.invalid")
            git("config", "user.name", "Fixture")
            (root / ".gitignore").write_text("metadata\nignored\n")
            source = root / "source"
            source.write_text("original")
            git("add", ".")
            git("commit", "-qm", "fixture")
            clean = status()
            self.assertFalse(clean["dirty"])
            self.assertEqual(status(), clean)
            (root / "ignored").write_text("ignored secrets")
            self.assertEqual(status(), clean)
            source.write_text("modified")
            dirty = status()
            self.assertTrue(dirty["dirty"])
            self.assertNotEqual(dirty["fingerprint"], clean["fingerprint"])
            git("add", "source")
            self.assertEqual(status(), dirty)
            source.write_text("modified again")
            self.assertNotEqual(status()["fingerprint"], dirty["fingerprint"])
            git("reset", "--hard", "HEAD")
            untracked = root / "untracked with spaces"
            untracked.write_text("first")
            first = status()
            untracked.write_text("second")
            self.assertNotEqual(status()["fingerprint"], first["fingerprint"])
            untracked.unlink()
            self.assertEqual(status(), clean)
            source.unlink()
            self.assertNotEqual(status()["fingerprint"], clean["fingerprint"])


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--candidate", required=True)
    OPTIONS, arguments = parser.parse_known_args()
    unittest.main(argv=[__file__, *arguments])
