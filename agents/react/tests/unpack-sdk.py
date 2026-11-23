"""Install a packed npm artifact under its package name for the browser consumer."""
import pathlib
import shutil
import sys
import tarfile
import tempfile

artifact, runtime, output = map(pathlib.Path, sys.argv[1:])
with tempfile.TemporaryDirectory(dir=output.parent) as temporary:
    with tarfile.open(artifact) as package:
        package.extractall(temporary, filter="data")
    destination = output / "node_modules" / "@pulsebeam" / "web"
    destination.parent.mkdir(parents=True)
    (pathlib.Path(temporary) / "package").rename(destination)
shutil.copytree(runtime, output / "node_modules" / "@ubjs" / "core")
