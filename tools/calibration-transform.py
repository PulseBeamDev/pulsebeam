"""Three finite, context-checked SFU defects, never a worktree editor."""
import argparse
import hashlib
import json
from pathlib import Path

TRANSFORMS = {
    "local-fanout": ("src/route/fabric/local.rs",
                     "for branch in self.branches.values_mut() {\n            branch.discard_stale(now, &self.fates, |_| true);",
                     "for branch in self.branches.values_mut().take(1) {\n            branch.discard_stale(now, &self.fates, |_| true);"),
    "remote-fanout": ("src/shard/router.rs",
                      "for destination in destinations {\n                if ordered.is_some_and(|unit| {",
                      "for destination in destinations.iter_mut().take(1) {\n                if ordered.is_some_and(|unit| {"),
    "materialization": ("src/control/controller.rs",
                        "let result = match (validation, materialized) {",
                        "let result = match (validation, materialized || pending.command.is_none()) {"),
}


def transform(variant, relative, data):
    path, before, after = TRANSFORMS[variant]
    if relative != path:
        return data
    text = data.decode()
    if text.count(before) != 1:
        raise ValueError(f"stale {variant} transform: expected one exact context in {path}")
    return text.replace(before, after, 1).encode()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--variant", choices=TRANSFORMS, required=True)
    parser.add_argument("--manifest", required=True)
    parser.add_argument("--provenance", required=True)
    args = parser.parse_args()
    inputs = json.loads(Path(args.manifest).read_text())
    site = TRANSFORMS[args.variant][0]
    if sum(entry["relative"] == site for entry in inputs) != 1:
        raise ValueError(f"missing or ambiguous mutation source: {site}")
    hashes = {}
    # Validate every transform before producing any outputs.
    outputs = []
    for entry in inputs:
        original = Path(entry["input"]).read_bytes()
        changed = transform(args.variant, entry["relative"], original)
        outputs.append((Path(entry["output"]), changed))
        hashes[entry["relative"]] = {"input": hashlib.sha256(original).hexdigest(),
                                     "output": hashlib.sha256(changed).hexdigest()}
    for path, data in outputs:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
    identity = {"variant": args.variant,
                "transform_sha256": hashlib.sha256(json.dumps(TRANSFORMS[args.variant]).encode()).hexdigest(),
                "files": hashes}
    Path(args.provenance).write_text(json.dumps(identity, sort_keys=True, indent=2) + "\n")


if __name__ == "__main__":
    main()
