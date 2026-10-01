"""Extract and validate the standalone SDK and its bundled auth dependency."""
import pathlib
import sys
import tarfile
import tomllib

archive, destination = (pathlib.Path(p) for p in sys.argv[1:])
destination.mkdir(parents=True, exist_ok=True)
with tarfile.open(archive) as package:
    package.extractall(destination, filter="data")
manifest = tomllib.loads((destination / "Cargo.toml").read_text())
assert manifest["package"]["name"] == "pulsebeam-server"
assert manifest["package"]["version"] == "0.1.0"
assert not manifest["package"].get("publish") is False
auth_dependency = manifest["dependencies"]["pulsebeam-auth"]
assert auth_dependency["path"] == "vendor/pulsebeam-auth"
auth_root = destination / auth_dependency["path"]
assert auth_root.resolve().is_relative_to(destination.resolve())
auth = tomllib.loads((auth_root / "Cargo.toml").read_text())
assert auth["package"]["name"] == "pulsebeam-auth"
assert auth["package"]["version"] == auth_dependency["version"]
for root, owner in [(destination, manifest), (auth_root, auth)]:
    assert "workspace" not in owner
    assert not any(isinstance(value, dict) and "workspace" in value
                   for value in owner["package"].values())
    for name, dependency in owner["dependencies"].items():
        if isinstance(dependency, dict):
            assert "workspace" not in dependency
            if root == destination and name == "pulsebeam-auth":
                continue
            assert "path" not in dependency
    assert (root / "README.md").is_file()
    assert (root / "LICENSE").is_file()
