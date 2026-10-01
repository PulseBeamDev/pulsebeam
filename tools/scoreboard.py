"""Explicitly regenerate the tracked deterministic simulation scoreboard."""

import argparse
import os
from pathlib import Path
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--runner", required=True)
    parser.add_argument("--parser", required=True)
    parser.add_argument("--runner-arg", action="append", default=[])
    parser.add_argument("--output", default="bwe-baseline.txt")
    args = parser.parse_args()
    runner = Path(args.runner).resolve()
    formatter = Path(args.parser).resolve()
    root = Path(os.environ["BUILD_WORKSPACE_DIRECTORY"])
    with subprocess.Popen([str(runner), *args.runner_arg], stdout=subprocess.PIPE, stderr=subprocess.STDOUT) as process:
        output = subprocess.run([str(formatter)], stdin=process.stdout, capture_output=True)
        process.stdout.close()
        status = process.wait()
    sys.stderr.buffer.write(output.stderr)
    if output.returncode:
        return output.returncode
    (root / args.output).write_bytes(output.stdout)
    print(f"Wrote {root / args.output}; simulation exit status: {status}")
    return status


if __name__ == "__main__":
    sys.exit(main())
