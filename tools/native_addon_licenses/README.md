# Native addon source notice inventory

Generate an offline inventory for the **source graph described by a verified Cargo.lock**. This tool is independent of the package build. Its output is a conservative review input, not a determination of license compliance or an attestation that any binary was built from these sources.

## Run

Use Python 3.12 or newer. Obtain the lockfile and all source inputs separately, verify their acquisition, and prepare an inputs JSON file. The lock hash is mandatory and must come from a separately trusted declaration, not a hash computed from the just-downloaded lock as its only verification. The tool never downloads files or reads Cargo's user cache.

```sh
python3.12 tools/native_addon_licenses/inventory.py \
  --lock /inputs/Cargo.lock \
  --lock-sha256 TRUSTED_LOWERCASE_SHA256 \
  --inputs /inputs/sources.json \
  --manifest /outputs/manifest.json \
  --notices /outputs/notices.json
```

Both output paths must not already exist. On success, `manifest.json` identifies every reached package, source, archive checksum, deterministic source tree digest, declared license, and discovered texts and their SHA-256 hashes. `notices.json` contains the full UTF-8 text of each listed notice, including its exact trailing newline state. Both are deterministic JSON with no local paths or timestamps. A nonzero exit is a failure, not an empty or partial inventory. Review both outputs before using them.

Example `sources.json`, with illustrative identities rather than an exhaustive list:

```json
{
  "sources": [
    {"name": "uniffi-runtime-napi", "version": "1.0.0", "directory": "runtime-napi"},
    {"name": "example", "version": "2.0.0", "source": "registry+https://github.com/rust-lang/crates.io-index", "archive": "example-2.0.0.crate"}
  ],
  "supplements": [
    {"name": "uniffi-runtime-napi", "version": "1.0.0", "files": ["reviewed/runtime-license.txt"]}
  ]
}
```

Paths resolve relative to the inputs file, or to `--base` when supplied. Declare **exactly** one source for every package in the reachable lock graph, matching name, version and exact lock `source`; omit `source` for a source-less workspace package. A registry package requires its original crate archive and the archive must match the lock checksum. Other archives also require a matching lock checksum. A directory may additionally accompany an archive only when every file exactly matches the archive. Workspace or git packages without a lock checksum require either a directory or a separately SHA-256-pinned workspace archive and a safe member prefix. The full source tree digest is recorded, but neither a directory's authenticity nor an archive's correspondence to a git revision is verified by this tool. Verify those inputs independently. Symlinks or special files inside source directories, unsafe archive paths, missing or unreadable files, and ambiguous lock references cause failure.

An explicitly reviewed supplement is additive, keyed by exact name and version, and must identify exactly one reachable package. Supply the full text, not an SPDX identifier or a URL. Use supplements for missing upstream license texts, inherited workspace license files outside a supplied crate directory, or other review discoveries. Entries for non-reachable or ambiguous identities fail. A root-level license text, root-level `LICENSES` text, declared local `license-file`, or supplement is required for each package; a nested vendored license or a NOTICE alone is insufficient.

## Review boundary

The graph walk follows all lockfile dependency edges without target, dependency-kind or feature filtering, so resolved build, proc-macro and other-platform dependencies are retained. It cannot include optional dependencies absent from the lockfile or unrecorded build-time downloads. Name-only or partially qualified dependency references must resolve uniquely; otherwise the tool stops rather than guessing.

All files under `LICENSES` directories and recursively named LICENSE, LICENCE, COPYING, COPYRIGHT, NOTICE, or UNLICENSE (also with punctuation-separated suffixes) are collected, including nested vendored directories such as libffi. A Cargo `license-file` is collected too. The scanner cannot infer every relevant text: licenses embedded in READMEs, source headers, differently named files, generated/downloaded code, or excluded crate archive contents require human review and explicit supplements. Inherited or parent-relative `license-file` references require a reviewed supplement rather than path traversal; missing local `license-file` references fail even if supplemented. Successful extraction does not mean that each nested component's obligations have been identified, and scanning is not an SPDX expression resolver.

The published `@ubjs/node-linux-x64-gnu@0.31.0-5` binary was compared byte-for-byte with the GitHub Actions artifact identified in the packaged provenance note. GitHub associates that artifact with revision `0f7fc67e8dd98ad43af3787d45c3c570d469a145`, whose pinned Cargo.lock SHA-256 is `dcf5d2ef5ccb916a75471225926fbab493828b4ea6fb07f1f707edd2690692f2`. This is stronger than the registry's `gitHead` field alone, but the platform package has no standalone signed build attestation. Successful source extraction is not a legal compliance determination.

## Bazel integration design

The Bazel owner fetches a SHA-256-pinned upstream source archive, its independently pinned lockfile, checksum-verified crate archives, and reviewed supplemental license texts. The inventory action receives those files as declared inputs and runs offline. The Node archive includes its manifest, notice texts, provenance limitation and the Rust 1.97.1 standard-library notices indicated by the reported source revision's publishing workflow. Review this source evidence against binary and build provenance before release; the inventory does not establish that correspondence.

## Tests

Run `./bazel test //tools/native_addon_licenses:inventory_test` from the repository root. The fixtures are synthetic and need no network or external crate cache.
