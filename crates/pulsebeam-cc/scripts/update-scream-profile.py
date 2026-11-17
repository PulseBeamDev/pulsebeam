"""Record selected SCReAM source identity, not behavioral conformance."""

import argparse
import hashlib
from pathlib import Path
import re
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source_root", type=Path, help="qualified libwebrtc checkout")
    args = parser.parse_args()
    source = args.source_root.resolve(strict=True)
    revision = subprocess.check_output(
        ["git", "-C", str(source), "rev-parse", "HEAD"], text=True
    ).strip()
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        parser.error("source root must identify an exact Git revision")
    status = subprocess.check_output(
        ["git", "-C", str(source), "status", "--porcelain", "--untracked-files=no"],
        text=True,
    )
    if status:
        parser.error("source root has tracked modifications")

    scripts = Path(__file__).resolve().parent
    verifier = scripts / "verify-scream-profile.sh"
    original = verifier.read_text()
    block = re.search(r"(?<=done <<'SOURCES'\n).*?(?=\nSOURCES\n)", original, re.S)
    if block is None:
        parser.error("verifier source manifest is missing")
    rows = []
    notices = {}
    for row in block.group().splitlines():
        _, _, relative = row.split()
        artifact = (source / relative).read_bytes()
        rows.append(f"{hashlib.sha256(artifact).hexdigest()} {len(artifact)} {relative}")
        if relative in ("LICENSE", "PATENTS"):
            notices[relative] = artifact
    updated = original[:block.start()] + "\n".join(rows) + original[block.end():]
    updated, count = re.subn(
        r"^revision='[0-9a-f]{40}'$", f"revision='{revision}'", updated, flags=re.M
    )
    if count != 1 or set(notices) != {"LICENSE", "PATENTS"}:
        parser.error("verifier revision or retained notices are incomplete")

    verifier.write_text(updated)
    for name, content in notices.items():
        (scripts.parent / "src/congestion/screamv2" / name).write_bytes(content)
    print(f"Recorded libwebrtc {revision}: {len(rows)} source artifacts.")
    print("Update port provenance comments and run source verification.")
    print("Run the native differential proof separately.")
    print("Source identity alone is not behavioral conformance.")


if __name__ == "__main__":
    main()
