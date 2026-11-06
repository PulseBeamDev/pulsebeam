import json
import os
from pathlib import Path
import unittest


class AspectTest(unittest.TestCase):
    def test_alias_configured_deps_and_proc_macros(self):
        root = Path(os.environ["TEST_SRCDIR"]) / os.environ["TEST_WORKSPACE"] / "tools/native_licenses"
        manifest = json.loads((root / "smoke.manifest.json").read_text(encoding="utf-8"))
        crates = manifest["crates"]
        names = {crate["name"] for crate in crates}
        self.assertTrue({"arrayvec", "serde", "serde_derive", "syn"}.issubset(names), names)
        self.assertNotIn("anyhow", names)
        self.assertEqual(len(crates), len({crate["repository"] for crate in crates}))
        self.assertIn("serde_derive", (root / "smoke.notices.txt").read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
