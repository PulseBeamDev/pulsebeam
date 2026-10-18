"""Regression tests for conservative Clippy source rewriting."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("clippy_fix", Path(__file__).resolve().parents[1] / "clippy-fix.py")
fixer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixer)


class ClippyFixTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "src").mkdir()
        self.source = self.root / "src/lib.rs"
        self.source.write_text("let unused = 1;\n")
        self.diagnostics = self.root / "diagnostics.json"

    def emit(self, spans):
        self.diagnostics.write_text(json.dumps({"$message_type": "diagnostic", "spans": spans}) + "\n")

    def span(self, start=4, end=10, replacement="used", applicability="MachineApplicable", name="src/lib.rs"):
        return dict(file_name=name, byte_start=start, byte_end=end,
                    suggested_replacement=replacement, suggestion_applicability=applicability)

    def test_machine_applicable(self):
        self.emit([self.span()])
        self.assertEqual(fixer.apply(self.root, [self.diagnostics]), 1)
        self.assertEqual(self.source.read_text(), "let used = 1;\n")

    def test_ambiguous_replacement_not_applied(self):
        self.emit([self.span(applicability="MaybeIncorrect")])
        self.assertEqual(fixer.apply(self.root, [self.diagnostics]), 0)

    def test_conflicts_not_applied(self):
        self.diagnostics.write_text("\n".join(
            json.dumps({"spans": [self.span(replacement=value)]}) for value in ["foo", "bar"]))
        self.assertEqual(fixer.apply(self.root, [self.diagnostics]), 0)

    def test_path_traversal_not_applied(self):
        self.emit([self.span(name="../elsewhere.rs")])
        self.assertEqual(fixer.apply(self.root, [self.diagnostics]), 0)

    def test_overlaps_not_applied(self):
        self.emit([self.span(), self.span(6, 11, "foo")])
        self.assertEqual(fixer.apply(self.root, [self.diagnostics]), 0)

    def test_duplicate_diagnostics_apply_once(self):
        self.emit([self.span()])
        self.assertEqual(fixer.apply(self.root, [self.diagnostics, self.diagnostics]), 1)


if __name__ == "__main__":
    unittest.main()
