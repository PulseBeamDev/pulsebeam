"""Keep generated distributions on the native workspace version."""

import json
from pathlib import Path
import sys
import tomllib


def main():
    workspace, npm, python = (Path(value) for value in sys.argv[1:])
    version = tomllib.loads(workspace.read_text())["workspace"]["package"]["version"]
    node = json.loads(npm.read_text())
    wheel = tomllib.loads(python.read_text())["project"]
    assert node["name"] == "@pulsebeam/agent"
    assert wheel["name"] == "pulsebeam-agent"
    assert node["version"] == version == wheel["version"], (
        version,
        node["version"],
        wheel["version"],
    )


if __name__ == "__main__":
    main()
