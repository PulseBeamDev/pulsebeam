"""The SFU consumes runtime services, never a backend scheduler directly."""

from pathlib import Path
import tomllib
import unittest


class RuntimeBoundaryTests(unittest.TestCase):
    def test_sfu_has_no_direct_backend_dependencies(self):
        manifest = Path(__file__).resolve().parents[2] / "crates/pulsebeam/Cargo.toml"
        document = tomllib.loads(manifest.read_text())

        def check_tables(table):
            for name, value in table.items():
                if name in ("dependencies", "dev-dependencies", "build-dependencies"):
                    for alias, dependency in value.items():
                        package = dependency.get("package", alias) if isinstance(dependency, dict) else alias
                        self.assertNotIn(package, ("tokio", "tokio-util"),
                                         "SFU runtime operations belong in pulsebeam-runtime")
                elif isinstance(value, dict):
                    check_tables(value)

        check_tables(document)


if __name__ == "__main__":
    unittest.main()
