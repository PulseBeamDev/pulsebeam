"""Exec the pinned ChromeDriver with retained launch diagnostics."""

import os
from pathlib import Path
import sys


def main():
    binary = os.environ["PULSEBEAM_CHROMEDRIVER_BINARY"]
    log = Path(os.environ["TEST_UNDECLARED_OUTPUTS_DIR"]) / f"chromedriver-{os.getpid()}.log"
    print(f"chromedriver-log: {log}", file=sys.stderr, flush=True)
    os.execv(binary, [binary, "--verbose", f"--log-path={log}", *sys.argv[1:]])


if __name__ == "__main__":
    main()
