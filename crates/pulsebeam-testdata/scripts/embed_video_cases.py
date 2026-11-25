"""Generate the compile-time input table for the isolated video replay."""

import csv
import json
from pathlib import Path
import sys


def embed(path, *, binary=False):
    macro = "include_bytes" if binary else "include_str"
    suffix = json.dumps("/" + path.as_posix(), ensure_ascii=True)
    return f'{macro}!(concat!(env!("CODEC_FIXTURES"), {suffix}))'


def generate(inventory):
    with inventory.open() as source:
        for row in csv.DictReader(source, delimiter="\t"):
            codec, mode = row["codec"], row["mode"]
            if mode == "-" or row["wire"] == "missing":
                continue
            wire = Path(row["wire"])
            pilot = wire.parts[0] == "vp9-pilot"
            fields = {
                "codec": json.dumps(codec),
                "mode": json.dumps(mode),
                "wire": embed(wire, binary=True),
                "index": embed(wire.with_suffix(".tsv")),
                "observations": embed(Path(row["fixed_target"])),
                "faults": embed(Path(row["loss"])),
                "encoded_index": embed(Path(row["sender"])),
                "encoded": embed(Path(row["sender"]).with_suffix(".bin"), binary=True) if pilot else "&[]",
            }
            yield "    Case { " + ", ".join(f"{field}: {value}" for field, value in fields.items()) + " },"


if __name__ == "__main__":
    inventory, output = map(Path, sys.argv[1:])
    output.write_text("&[\n" + "\n".join(generate(inventory)) + "\n]\n")
