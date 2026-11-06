"""Pinned source inputs for the separately distributed Node N-API runtime."""

ADDON_LOCK_SHA256 = "dcf5d2ef5ccb916a75471225926fbab493828b4ea6fb07f1f707edd2690692f2"
_SOURCE_REVISION = "0f7fc67e8dd98ad43af3787d45c3c570d469a145"
_SOURCE_SHA256 = "8211d3faea7cba97ce9a46bf1ef119121700fb98e2d926e1a3ee49629479901b"

# These crates omit their project-root license from the published crate archive.
# The texts are pinned to each crate's recorded .cargo_vcs_info.json revision.
_SUPPLEMENTS = {
    "dlopen2@0.7.0": ["supp/dlopen2.txt"],
    "dlopen2_derive@0.4.3": ["supp/dlopen2.txt"],
    "libffi@5.1.1": ["supp/libffi.txt"],
    "libffi-sys@4.2.0": ["supp/libffi.txt"],
    "napi@2.16.17": ["supp/napi.txt"],
    "napi-build@2.4.0": ["supp/napi.txt"],
    "napi-derive@2.16.13": ["supp/napi.txt"],
    "napi-derive-backend@1.0.75": ["supp/napi.txt"],
    "napi-sys@2.4.0": ["supp/napi.txt"],
    # These platform-only import-library crates share winapi-rs's MIT license.
    "winapi-i686-pc-windows-gnu@0.4.0": ["supp/winapi.txt"],
    "winapi-x86_64-pc-windows-gnu@0.4.0": ["supp/winapi.txt"],
    "uniffi-runtime-core@0.1.0": ["supp/upstream-license.txt", "supp/MPL-2.0.txt"],
    "uniffi-runtime-napi@0.1.0": ["supp/upstream-license.txt", "supp/MPL-2.0.txt"],
}

def _addon_sources_impl(ctx):
    base = "https://raw.githubusercontent.com/jhugman/uniffi-bindgen-react-native/" + _SOURCE_REVISION + "/"
    downloads = [
        ctx.download(base + "Cargo.lock", "Cargo.lock", sha256 = ADDON_LOCK_SHA256, block = False),
        ctx.download("https://codeload.github.com/jhugman/uniffi-bindgen-react-native/tar.gz/" + _SOURCE_REVISION, "upstream.tar.gz", sha256 = _SOURCE_SHA256, block = False),
        ctx.download(base + "LICENSE", "supp/upstream-license.txt", sha256 = "d1cc4c0ada218e8a336dc8c3d8bb7d5b1c8dedfa31f5b25c9ea1759d21b8b62d", block = False),
        ctx.download("https://raw.githubusercontent.com/OpenByteDev/dlopen2/617ed96425729a5a10f8e55ece8d1570acd6353a/LICENSE", "supp/dlopen2.txt", sha256 = "39fa265207450e77c62e90c5594a06c085b655d8374c7ced4bf7894b6bd95dd2", block = False),
        ctx.download("https://raw.githubusercontent.com/libffi-rs/libffi-rs/463a6cbde4abaca2bd164ea3c5a06aa329781a36/LICENSE-MIT", "supp/libffi.txt", sha256 = "7dafbced615981b7969f8d74f89cb6ecfca513aa4df68c2dc8812807a79b2594", block = False),
        ctx.download("https://raw.githubusercontent.com/napi-rs/napi-rs/f2178312d0e3e07beecc19836b91716a229107d3/LICENSE", "supp/napi.txt", sha256 = "3f1ce66533302df3a32edbfdfc0b78f0dd34659e4c1f5817162e5ea3c2297215", block = False),
        ctx.download("https://raw.githubusercontent.com/retep998/winapi-rs/796a8e6c2971dc2ff1bcff166e6671284f9b5b6b/LICENSE-MIT", "supp/winapi.txt", sha256 = "ce7bc3499fee93d5022ef430d5e4201e79a6d9154f3974e42f41349f0569e09b", block = False),
        ctx.download("https://static.rust-lang.org/dist/2026-07-16/rustc-1.97.1-x86_64-unknown-linux-gnu.tar.xz", "rustc.tar.xz", sha256 = "9819d0a32d56bd339585319c80260e332779f5541fd66838ab7e016d6c814819", block = False),
    ]
    for download in downloads:
        download.wait()
    ctx.file("supp/MPL-2.0.txt", ctx.read(ctx.attr.mpl))
    ctx.read(ctx.attr.inventory)
    result = ctx.execute([str(ctx.path(ctx.attr.python)), "-B", str(ctx.path(ctx.attr.graph)), str(ctx.path("Cargo.lock"))])
    if result.return_code:
        fail("cannot resolve addon Cargo.lock: " + result.stderr)
    packages = json.decode(result.stdout)
    sources = []
    pending = []
    for package in packages:
        name = package["name"]
        version = package["version"]
        source = package.get("source")
        spec = {"name": name, "version": version}
        if source == "registry+https://github.com/rust-lang/crates.io-index":
            archive = "crates/" + name + "-" + version + ".crate"
            pending.append(ctx.download("https://static.crates.io/crates/" + name + "/" + name + "-" + version + ".crate", archive, sha256 = package["checksum"], block = False))
            spec.update({"source": source, "archive": archive})
        elif source == None and name in ["uniffi-runtime-core", "uniffi-runtime-napi"]:
            spec.update({
                "workspace_archive": "upstream.tar.gz",
                "archive_sha256": _SOURCE_SHA256,
                "prefix": "uniffi-bindgen-react-native-" + _SOURCE_REVISION + "/runtimes/" + name[len("uniffi-runtime-"):],
            })
        else:
            fail("unsupported addon source identity: " + str(package))
        sources.append(spec)
    for download in pending:
        download.wait()
    supplements = []
    for identity, files in _SUPPLEMENTS.items():
        name, version = identity.split("@")
        supplements.append({"name": name, "version": version, "files": files})
    ctx.file("sources.json", json.encode({"sources": sources, "supplements": supplements}))
    extract = ctx.execute([
        str(ctx.path(ctx.attr.python)),
        "-B",
        "-c",
        "import pathlib,tarfile; t=tarfile.open('rustc.tar.xz'); m=next(x for x in t.getmembers() if x.name.endswith('/rustc/share/doc/rust/COPYRIGHT-library.html')); pathlib.Path('NODE_ADDON_RUST_STANDARD_LIBRARY_NOTICES.html').write_bytes(t.extractfile(m).read())",
    ])
    if extract.return_code:
        fail("cannot extract upstream Rust standard-library notices: " + extract.stderr)
    ctx.delete("rustc.tar.xz")
    ctx.file("NODE_ADDON_SOURCE_PROVENANCE.txt", """The @ubjs/node-linux-x64-gnu 0.31.0-5 binary has SHA-256
7cef651b6ab0b45ca7992bfb1a236d956f053d9f95a186f04ec763769c22a0c4.
It was independently compared byte-for-byte with GitHub Actions artifact
9449636375, whose GitHub-reported archive SHA-256 is
d34a133b75099a46872b99602256708da89104a7f6a0e5be38ce7226363b2fd2.
GitHub associates that artifact with workflow run 32490613485 and source
revision 0f7fc67e8dd98ad43af3787d45c3c570d469a145. The source workflow
specifies Rust 1.97.1. The @ubjs/node root package has a provenance statement
identifying the same run and revision; its signature was not verified here.
The platform package has no standalone signed build attestation. The Cargo
inventory conservatively includes all resolved target and build dependencies.
""")
    ctx.file("BUILD.bazel", """package(default_visibility = ["//visibility:public"])
exports_files(["Cargo.lock", "sources.json", "NODE_ADDON_RUST_STANDARD_LIBRARY_NOTICES.html", "NODE_ADDON_SOURCE_PROVENANCE.txt"])
filegroup(name = "inputs", srcs = glob(["crates/**", "supp/**"]) + ["Cargo.lock", "sources.json", "upstream.tar.gz"])
""")
    return ctx.repo_metadata(reproducible = True)

addon_sources = repository_rule(
    implementation = _addon_sources_impl,
    attrs = {
        "graph": attr.label(mandatory = True, allow_single_file = True),
        "inventory": attr.label(mandatory = True, allow_single_file = True),
        "mpl": attr.label(mandatory = True, allow_single_file = True),
        "python": attr.label(mandatory = True, allow_single_file = True),
    },
)
