"""Run the upstream GN-built CXX generator without its Rust runtime edge."""
from pathlib import Path
import subprocess
import sys

exe, source, header, implementation = sys.argv[1:]
for output, flags in [(header, ["--header"]), (implementation, [])]:
    Path(output).parent.mkdir(parents=True, exist_ok=True)
    with open(output, "wb") as stream:
        subprocess.run([exe, source, *flags], stdout=stream, check=True)
