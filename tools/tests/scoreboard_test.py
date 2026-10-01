"""Contract for explicit replay arguments and scoreboard outcome propagation."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "scoreboard.py"
PARSER = Path("tools/scoreboard_parser").resolve()
RUNNER = """import json
import os
from pathlib import Path
import sys

Path(os.environ["CAPTURE"]).write_text(json.dumps(sys.argv[1:]))
status = int(os.environ["SIM_STATUS"])
if os.environ["SIM_RECORDS"] == "1":
    print("[scoreboard] throughput=123 elapsed=1.234ms")
    print("[simulation-result] " + ("FAIL" if status else "PASS") + " tests::smoke")
sys.exit(status)
"""


class ScoreboardContract(unittest.TestCase):
    def invoke(self, status=0, records=True):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            runner = root / "runner.py"
            runner.write_text(RUNNER)
            replay_args = ["fake-libtest", "--mode", "all", "--jobs", "1", "--skip", "tests::exception"]
            result = subprocess.run(
                [sys.executable, str(SCRIPT), "--runner", sys.executable,
                 "--parser", str(PARSER), "--output", "baseline",
                 "--runner-arg=" + str(runner)] + ["--runner-arg=" + arg for arg in replay_args],
                env=os.environ | {
                    "BUILD_WORKSPACE_DIRECTORY": str(root),
                    "CAPTURE": str(root / "args.json"),
                    "SIM_STATUS": str(status),
                    "SIM_RECORDS": str(int(records)),
                },
                capture_output=True, text=True,
            )
            output = (root / "baseline").read_text() if (root / "baseline").exists() else None
            return result, output, json.loads((root / "args.json").read_text()), replay_args

    def test_replay_arguments_and_normalised_metrics(self):
        result, output, arguments, expected = self.invoke()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(arguments, expected)
        self.assertEqual(output, "tests::smoke\n  [PASS]\n  throughput=123 elapsed=<ms>\n\n")

    def test_simulation_failure_is_not_hidden_by_successful_parser(self):
        result, output, _, _ = self.invoke(status=7)
        self.assertEqual(result.returncode, 7)
        self.assertIn("[FAIL]", output)

    def test_missing_records_fail_without_writing_baseline(self):
        result, output, _, _ = self.invoke(records=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no plans found", result.stderr)
        self.assertIsNone(output)


if __name__ == "__main__":
    unittest.main()
