"""Keep GNU release binaries independent of the provisioning environment."""

import argparse
import re
import subprocess


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True)
    parser.add_argument("--readelf", required=True)
    args = parser.parse_args()
    metadata = subprocess.check_output(
        [args.readelf, "--program-headers", "--dynamic", "--version-info", args.binary],
        text=True,
    )
    comments = subprocess.check_output([args.readelf, "--string-dump=.comment", args.binary], text=True)
    assert re.search(r"\bmold [0-9]+\.[0-9]+", comments), "native Rust binary was not linked by mold"
    assert "/nix/store/" not in metadata, "release ELF metadata requires the Nix store"
    assert "RPATH" not in metadata and "RUNPATH" not in metadata, "standalone release must not need a build-directory search path"
    interpreters = re.findall(r"Requesting program interpreter: ([^\]]+)", metadata)
    assert interpreters in [["/lib64/ld-linux-x86-64.so.2"], ["/lib/ld-linux-aarch64.so.1"]], interpreters
    # Do not raise runtime requirements above the Ubuntu 24.04 host baseline.
    limits = {"GLIBC": (2, 39), "GLIBCXX": (3, 4, 32)}
    for namespace, limit in limits.items():
        versions = [tuple(map(int, value.split("."))) for value in re.findall(rf"Name: {namespace}_([0-9.]+)", metadata)]
        assert all(version <= limit for version in versions), (namespace, versions, limit)
    subprocess.run([args.binary, "--version"], check=True)
    print("Mold linkage, GNU runtime baseline and provisioning-independent ELF metadata passed.")


if __name__ == "__main__":
    main()
