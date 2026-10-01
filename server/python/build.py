"""Invoke the standard backend against declared source, without metadata duplication."""
import os
import pathlib
import shutil
import sys
import tempfile

from setuptools import build_meta

manifest, output = (pathlib.Path(p).resolve() for p in sys.argv[1:])
source = manifest.parent
output.mkdir(parents=True, exist_ok=True)
os.environ["SOURCE_DATE_EPOCH"] = "0"
with tempfile.TemporaryDirectory() as directory:
    work = pathlib.Path(directory)
    for name in ("pyproject.toml", "README.md", "LICENSE"):
        shutil.copyfile(source / name, work / name)
    shutil.copytree(source / "pulsebeam_server", work / "pulsebeam_server", ignore=shutil.ignore_patterns("__pycache__"))
    os.chdir(work)
    build_meta.build_wheel(str(output))
    build_meta.build_sdist(str(output))
