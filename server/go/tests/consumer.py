"""Consume a source module artifact through a normal offline Go module import."""
import os
import pathlib
import subprocess
import sys
import tarfile
import tempfile

archive, go, tests, vectors = (pathlib.Path(p).resolve() for p in sys.argv[1:])
with tempfile.TemporaryDirectory() as directory:
    root = pathlib.Path(directory)
    module = root / "module"
    module.mkdir()
    with tarfile.open(archive) as package:
        package.extractall(module, filter="data")
    consumer = root / "consumer"
    consumer.mkdir()
    (consumer / "go.mod").write_text(
        "module consumer\n\ngo 1.24.0\n\n"
        "require github.com/PulseBeamDev/pulsebeam/server/go v0.0.0\n"
        f"replace github.com/PulseBeamDev/pulsebeam/server/go => {module}\n"
    )
    (consumer / "auth_test.go").write_bytes(tests.read_bytes())
    env = {**os.environ, "GOROOT": str(go.parent.parent), "GOPROXY": "off", "GOSUMDB": "off", "GOTOOLCHAIN": "local", "CGO_ENABLED": "0", "GOCACHE": str(root / "cache"), "GOMODCACHE": str(root / "modules"), "PULSEBEAM_VECTORS": str(vectors)}
    subprocess.run([str(go), "test", "./..."], cwd=consumer, env=env, check=True)
