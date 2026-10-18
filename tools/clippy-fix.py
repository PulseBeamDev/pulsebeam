"""Apply only unambiguous machine-applicable Clippy suggestions from Bazel JSON."""
import json
from pathlib import Path


def suggestions(root: Path, diagnostics: list[Path]) -> dict[Path, list[tuple[int, int, bytes]]]:
    edits = {}
    root = root.resolve()
    for diagnostic_file in diagnostics:
        for line in diagnostic_file.read_text().splitlines():
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            if event.get("reason") == "compiler-message":
                event = event.get("message", {})
            if event.get("$message_type") not in (None, "diagnostic"):
                continue
            messages = [event]
            while messages:
                message = messages.pop()
                messages.extend(message.get("children", []))
                spans = message.get("spans", [])
                # A multipart suggestion must be applied atomically; do not
                # guess if some of its spans are unsupported.
                replacements = [s for s in spans if s.get("suggested_replacement") is not None]
                if not replacements:
                    continue
                if any(s.get("suggestion_applicability") != "MachineApplicable" for s in replacements):
                    continue
                group = []
                for span in replacements:
                    name = span.get("file_name", "")
                    if not name or Path(name).is_absolute():
                        group = []
                        break
                    path = (root / name).resolve()
                    if not path.is_relative_to(root) or not path.is_file() or path.suffix != ".rs":
                        group = []
                        break
                    start, end = span.get("byte_start"), span.get("byte_end")
                    if not isinstance(start, int) or not isinstance(end, int) or start < 0 or end < start:
                        group = []
                        break
                    group.append((path, start, end, span["suggested_replacement"].encode()))
                if group and len(group) == len(replacements):
                    for path, start, end, replacement in group:
                        edits.setdefault(path, []).append((start, end, replacement))
    return edits


def apply(root: Path, diagnostic_files: list[Path]) -> int:
    count = 0
    for path, candidates in suggestions(root, diagnostic_files).items():
        original = path.read_bytes()
        # Duplicate diagnostics are frequent when the same crate is built under
        # different configurations. Reject conflicting replacements.
        grouped = {}
        for start, end, replacement in candidates:
            grouped.setdefault((start, end), set()).add(replacement)
        if any(len(values) != 1 for values in grouped.values()):
            continue
        edits = sorted((start, end, next(iter(values))) for (start, end), values in grouped.items())
        if any(end > len(original) for _, end, _ in edits):
            continue
        if any(a[1] > b[0] or a[0] == b[0] for a, b in zip(edits, edits[1:])):
            continue
        result = original
        for start, end, replacement in reversed(edits):
            result = result[:start] + replacement + result[end:]
        if result != original:
            path.write_bytes(result)
            count += len(edits)
    return count
