"""Install the React and Web tarballs in an isolated consumer."""
import pathlib
import subprocess
import sys
import tarfile
import tempfile

react, web, runner, *dependencies = (str(pathlib.Path(p).resolve()) for p in sys.argv[1:])
with tempfile.TemporaryDirectory() as directory:
    root = pathlib.Path(directory)
    for name, artifact in (("react", react), ("web", web)):
        destination = root / "node_modules" / "@pulsebeam" / name
        destination.mkdir(parents=True)
        with tarfile.open(artifact) as package:
            package.extractall(destination, filter="data")
        # npm tarballs have a package/ root, unlike an installed node_modules entry.
        for child in (destination / "package").iterdir():
            child.rename(destination / child.name)
        (destination / "package").rmdir()
    subprocess.run([runner, directory, *dependencies], check=True)
