# Configured Rust license inventory

For a configured Rust target, load `//tools/native_licenses:inventory.bzl` and
add a rule in the consumer's BUILD file:

```starlark
load("//tools/native_licenses:inventory.bzl", "native_license_inventory")

native_license_inventory(
    name = "rust_licenses",
    roots = [":my_rust_target"],
    # Optional, reviewed text for a particular external crate repository:
    # supplemental = {"//legal:license": "rules_rust++crate+crates__name-version|name|version"},
)
```

Build `:rust_licenses` under the intended configuration. The default outputs
are `rust_licenses.manifest.json` and `rust_licenses.notices.txt`. Specify the
actual canonical repository name from the configured graph, not the example
above. Supplemental labels must each resolve to exactly one nonempty UTF-8
file and match the Cargo package name, version and canonical repository exactly.
Unmatched or duplicate entries fail the build; supplements cannot replace a
missing declared `license-file`.

The aspect follows configured `deps`, `proc_macro_deps`, and aliases, and
collects crate-universe `rust_*` target `compile_data`, merging targets from the
same external repository. It includes host proc macros and nested bundled
license and notice files visible in that data. It requires a root `Cargo.toml`,
license metadata, and license text for each crate. Text is UTF-8; license and
notice names are recognized by common prefixes or an explicit `license-file`.
The rule fails rather than guessing when data is missing. The manifest includes
paths and SHA-256 hashes of normalized text, not action-local source paths.

This is an inventory of Rust crate repositories exposed by the pinned
crate-universe conventions, not a complete distribution compliance review.
It excludes local Rust crates, native WebRTC, system libraries, build-only tools
not reachable through the traversed attributes, and files absent from
`compile_data`. Review bundled source and license obligations separately. Do
not interpret its inclusion of proc macros as evidence they ship in a binary.
