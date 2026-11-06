import json
from pathlib import Path
import tempfile
import unittest

from tools.native_licenses.generator import InventoryError, generate


class GeneratorTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.repo = "rules_rust++crate+crate__1.0.0"
        self.files = {}
        self.put("Cargo.toml", '[package]\nname = "crate"\nversion = "1.0.0"\nlicense = "MIT"\n')
        self.put("LICENSE-MIT", "MIT terms\n")

    def put(self, name, content):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")
        self.files[name] = str(path)

    def record(self):
        return {"repository": self.repo, "target": "ignored", "files": list(self.files.items())}

    def run_generator(self, records=None, supplemental=()):
        return generate({"records": records if records is not None else [self.record()], "supplemental": list(supplemental)})

    def supplement(self, spec=None, content="extra terms\n"):
        path = self.root / "supplement"
        path.write_text(content, encoding="utf-8")
        return {"spec": spec or self.repo + "|crate|1.0.0", "path": str(path), "label": "//legal:license"}

    def test_deterministic_dedup_and_nested_notices(self):
        self.put("vendor/lib/NOTICE.txt", "bundled notice\n")
        self.put("vendor/lib/LICENSE", "bundled license\n")
        first = self.record()
        second = dict(first, files=list(reversed(first["files"])))
        manifest, notices = self.run_generator([first, second])
        self.assertEqual((manifest, notices), self.run_generator([second, first]))
        self.assertEqual(len(json.loads(manifest)["crates"]), 1)
        self.assertIn("bundled notice", notices)
        self.assertIn("bundled license", notices)

    def test_supplement_exact_identity(self):
        del self.files["LICENSE-MIT"]
        manifest, notices = self.run_generator(supplemental=[self.supplement()])
        self.assertIn("extra terms", notices)
        self.assertEqual(json.loads(manifest)["crates"][0]["texts"][0]["kind"], "supplemental")
        for spec in (self.repo + "|crate|2.0.0", self.repo + "|other|1.0.0", "other|crate|1.0.0"):
            with self.subTest(spec=spec), self.assertRaises(InventoryError):
                self.run_generator(supplemental=[self.supplement(spec)])

    def test_missing_and_empty(self):
        for name in ("Cargo.toml", "LICENSE-MIT"):
            with self.subTest(name=name):
                saved = self.files.pop(name)
                with self.assertRaises(InventoryError):
                    self.run_generator()
                self.files[name] = saved
                original = Path(saved).read_text()
                Path(saved).write_text("  \n")
                with self.assertRaises(InventoryError):
                    self.run_generator()
                Path(saved).write_text(original)
        self.put("Cargo.toml", '[package]\nname="crate"\nversion="1.0.0"\n')
        with self.assertRaises(InventoryError):
            self.run_generator()

    def test_unmatched_supplement_is_rejected_even_with_license(self):
        with self.assertRaisesRegex(InventoryError, "unmatched supplemental"):
            self.run_generator(supplemental=[self.supplement(self.repo + "|crate|2.0.0")])

    def test_invalid_package_fields(self):
        for metadata in (
            'name=""\nversion="1.0.0"\nlicense="MIT"',
            'name="crate"\nversion=""\nlicense="MIT"',
            'name="crate"\nversion="1.0.0"\nlicense=""',
            'name="crate"\nversion="1.0.0"\nlicense-file="../LICENSE"',
        ):
            with self.subTest(metadata=metadata):
                self.put("Cargo.toml", "[package]\n" + metadata + "\n")
                with self.assertRaises(InventoryError):
                    self.run_generator()

    def test_nested_license_does_not_cover_crate(self):
        del self.files["LICENSE-MIT"]
        self.put("vendor/LICENSE", "vendor terms")
        with self.assertRaisesRegex(InventoryError, "no license text"):
            self.run_generator()
        self.put("NOTICE", "notice only")
        with self.assertRaisesRegex(InventoryError, "no license text"):
            self.run_generator()

    def test_declared_file_cannot_be_replaced_by_supplement(self):
        self.put("Cargo.toml", '[package]\nname="crate"\nversion="1.0.0"\nlicense-file="legal/terms.txt"\n')
        with self.assertRaisesRegex(InventoryError, "declared license-file"):
            self.run_generator(supplemental=[self.supplement()])
        self.put("legal/terms.txt", "custom terms")
        manifest, notices = self.run_generator()
        self.assertIn("custom terms", notices)
        self.assertEqual(json.loads(manifest)["crates"][0]["license_file"], "legal/terms.txt")

    def test_rejects_bad_inputs(self):
        with self.assertRaises(InventoryError):
            self.run_generator(records=[])
        with self.assertRaises(InventoryError):
            self.run_generator(supplemental=[self.supplement("bad")])
        item = self.supplement()
        with self.assertRaises(InventoryError):
            self.run_generator(supplemental=[item, item])
        with self.assertRaises(InventoryError):
            self.run_generator(supplemental=[self.supplement(content=" ")])
        record = self.record()
        record["files"].append(("../LICENSE", "elsewhere"))
        with self.assertRaises(InventoryError):
            self.run_generator([record])
        record = self.record()
        record["files"].append(("LICENSE-MIT", "elsewhere"))
        with self.assertRaises(InventoryError):
            self.run_generator([record])


if __name__ == "__main__":
    unittest.main()
