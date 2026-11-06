"""Print the conservative addon Cargo.lock closure for the pinned source fetch."""

import json
from pathlib import Path
import sys
import tomllib

import inventory

lock = tomllib.loads(Path(sys.argv[1]).read_text())
packages = inventory.dependencies(lock['package'], 'uniffi-runtime-napi')
print(json.dumps(packages, sort_keys=True))
