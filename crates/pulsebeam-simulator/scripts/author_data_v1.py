"""Export reviewed-candidate DATA_V1 receipts from the mixed-carrier simulator."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile


PREFIX = "[simulation-generated] data-v1 "
MODES = {"Latest", "Reliable", "Unreliable"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--runner", required=True)
    parser.add_argument("--sim-binary", required=True)
    parser.add_argument("--candidate", required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()

    with tempfile.TemporaryDirectory() as root:
        result = subprocess.run(
            [sys.executable, args.runner, args.sim_binary, "--candidate", args.candidate,
             "--filter", "sctp_and_quic_exchange", "--jobs", "1", "--artifacts-dir", root],
            env=os.environ | {"PULSEBEAM_AUTHOR_DATA_V1": "1"},
            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=120,
        )
        if result.returncode:
            sys.stderr.write(result.stdout)
            result.check_returncode()

    cases = [json.loads(line.removeprefix(PREFIX)) for line in result.stdout.splitlines()
             if line.startswith(PREFIX)]
    if len(cases) != len(MODES) or {case["delivery"] for case in cases} != MODES:
        raise ValueError("expected exactly one successful mixed-carrier receipt per delivery mode")
    candidate = {"version": 1, "cases": sorted(cases, key=lambda case: case["delivery"])}
    out = Path(os.environ.get("BUILD_WORKING_DIRECTORY", ".")) / args.out
    with out.open("x") as output:
        output.write(json.dumps(candidate, sort_keys=True, indent=2) + "\n")


if __name__ == "__main__":
    main()
