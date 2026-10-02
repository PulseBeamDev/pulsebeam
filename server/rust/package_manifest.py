"""Relocate the SDK dependency to the shared crate bundled in its archive."""
import pathlib
import sys
import tomllib

source, auth_source, output = (pathlib.Path(p) for p in sys.argv[1:])
text = source.read_text()
manifest = tomllib.loads(text)
auth = tomllib.loads(auth_source.read_text())
dependency = manifest["dependencies"]["pulsebeam-auth"]
assert auth["package"]["name"] == "pulsebeam-auth"
assert dependency["version"] == auth["package"]["version"]
original = f'path = "{dependency["path"]}"'
assert text.count(original) == 1
text = text.replace(original, 'path = "vendor/pulsebeam-auth"')
output.parent.mkdir(parents=True, exist_ok=True)
output.write_text(text)
