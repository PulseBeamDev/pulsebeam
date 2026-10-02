"""Install the publishable npm tarball into an isolated consumer."""
import pathlib
import subprocess
import sys
import tarfile
import tempfile

archive, runner, vectors = (str(pathlib.Path(p).resolve()) for p in sys.argv[1:])
with tempfile.TemporaryDirectory() as directory:
    with tarfile.open(archive) as package:
        package.extractall(directory, filter="data")
    subprocess.run([runner, str(pathlib.Path(directory) / "package"), vectors], check=True)
