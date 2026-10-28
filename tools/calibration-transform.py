"""Three finite, context-checked SFU defects, never a worktree editor."""
import argparse
import hashlib
import json
from pathlib import Path

TRANSFORMS = {
    "local-fanout": ("src/shard/router.rs",
                     "for &subscriber in &plan.local {\n        deliver(subscriber);\n    }",
                     "for &subscriber in plan.local.iter().take(1) {\n        deliver(subscriber);\n    }"),
    "remote-fanout": ("src/shard/router.rs",
                      "for remote in &plan.remote {\n        let env = Envelope::media(*remote, *link_seq, playout);",
                      "for remote in plan.remote.iter().take(1) {\n        let env = Envelope::media(*remote, *link_seq, playout);"),
    "materialization": ("src/control/controller.rs",
                        "let result = if materialized {\n                    let transport = pending.transport;",
                        "let result = if materialized || sent {\n                    let transport = pending.transport;"),
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
