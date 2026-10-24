"""Pinned WebRTC acquisition. GN generation and compilation are build actions."""

load("@rules_nixpkgs_core//:nixpkgs.bzl", "nixpkgs_package")
load(":runtime_deps.bzl", "DESKTOP_RUNTIME_SHA256")

_WEBRTC = "ba469aa2093ba950066258ca0a59a6fbd1295582"
_DEPOT_TOOLS = "ed9c87f6f12f6b87210e7025d4a36a5a72a2ccd4"
_LIBVPX = "31af37b1bd2774d11a932c1cd9a3849328375f64"

def _run(ctx, args, cwd = "", env = {}):
    result = ctx.execute(args, working_directory = cwd, environment = env, timeout = 3600, quiet = False)
    if result.return_code:
        fail("WebRTC acquisition failed: %s\n%s\n%s" % (args, result.stdout[-4000:], result.stderr[-4000:]))

def _desktop_runtime(ctx):
    urls = {}
    for url in ctx.read("checkout/src/build/linux/sysroot_scripts/generated_package_lists/bullseye.amd64").splitlines():
        package = url.split("/")[-1].split("_")[0]
        if package in DESKTOP_RUNTIME_SHA256:
            if package in urls:
                fail("Duplicate desktop runtime package in pinned sysroot: " + package)
            urls[package] = url
    for package, sha256 in DESKTOP_RUNTIME_SHA256.items():
        if package not in urls:
            fail("Desktop runtime package missing from pinned sysroot: " + package)
        directory = "runtime_debs/" + package
        ctx.download_and_extract(url = urls[package], sha256 = sha256, output = directory, type = "deb")
        payloads = [path for path in ctx.path(directory).readdir() if path.basename.startswith("data.tar.")]
        if len(payloads) != 1:
            fail("Expected one Debian runtime payload for " + package)
        ctx.extract(payloads[0], output = "desktop_runtime")

def _upstream_impl(ctx):
    if ctx.os.name != "linux" or ctx.os.arch not in ["amd64", "x86_64"]:
        fail("WebRTC source builds currently require Linux x86_64.")
    git = ctx.which("git")
    python = ctx.which("python3")
    if not git or not python:
        fail("WebRTC acquisition requires git and python3 on PATH.")
    _run(ctx, [git, "init", "depot_tools"])
    _run(ctx, [git, "-C", "depot_tools", "fetch", "--depth=1", "https://chromium.googlesource.com/chromium/tools/depot_tools.git", _DEPOT_TOOLS])
    _run(ctx, [git, "-C", "depot_tools", "checkout", "--detach", "FETCH_HEAD"])
    ctx.file("checkout/.gclient", "solutions = [{'name': 'src', 'url': 'https://github.com/webrtc-sdk/webrtc.git', 'deps_file': 'DEPS', 'managed': False, 'custom_deps': {}, 'custom_vars': {'checkout_instrumented_libraries': False}}]\n")
    env = {
        "PATH": str(ctx.path("depot_tools")) + ":" + ctx.os.environ.get("PATH", ""),
        "DEPOT_TOOLS_UPDATE": "0",
        "GCLIENT_PY3": "1",
        "VPYTHON_VIRTUALENV_ROOT": str(ctx.path("vpython")),
    }
    gclient = str(ctx.path("depot_tools/gclient.py"))
    _run(ctx, [python, gclient, "sync", "--no-history", "--shallow", "--nohooks", "--revision", "src@" + _WEBRTC], str(ctx.path("checkout")), env)
    clang = "checkout/src/third_party/llvm-build/Release+Asserts"
    clang_version = "llvmorg-23-init-10931-g20b6ec66-11"
    ctx.download_and_extract(
        url = "https://commondatastorage.googleapis.com/chromium-browser-clang/Linux_x64/clang-" + clang_version + ".tar.xz",
        sha256 = "de584381536aa5ba2403033c4f8b70f3c39c2e5d7fa87c953b7fd8bfbba0ee2a",
        output = clang,
    )
    ctx.file(clang + "/cr_build_revision", clang_version + "\n")
    sysroot_hash = "52d61d4446ffebfaa3dda2cd02da4ab4876ff237853f46d273e7f9b666652e1d"
    sysroot_url = "https://commondatastorage.googleapis.com/chrome-linux-sysroot/" + sysroot_hash
    sysroot = "checkout/src/build/linux/debian_bullseye_amd64-sysroot"
    ctx.download_and_extract(url = sysroot_url, sha256 = sysroot_hash, output = sysroot, type = "tar.xz")
    ctx.file(sysroot + "/.stamp", sysroot_url)
    _run(ctx, [python, gclient, "runhooks"], str(ctx.path("checkout")), env)
    result = ctx.execute([git, "-C", "checkout/src/third_party/libvpx/source/libvpx", "rev-parse", "HEAD"])
    if result.return_code or result.stdout.strip() != _LIBVPX:
        fail("WebRTC DEPS libvpx pin differs from the qualified revision " + _LIBVPX)
    for patch in ctx.attr.patches:
        _run(ctx, [git, "apply", str(ctx.path(patch))], str(ctx.path("checkout/src")))
    _run(ctx, [git, "apply", str(ctx.path(ctx.attr.libvpx_patch))], str(ctx.path("checkout/src/third_party/libvpx/source/libvpx")))
    # Foreign Bazel metadata must not create subpackage holes in the GN input tree.
    _run(ctx, [python, "-c", "from pathlib import Path; [(p.rename(p.with_name(p.name + '.upstream'))) for p in Path('checkout/src').rglob('*') if p.name in ('BUILD', 'BUILD.bazel') and p.is_file()]"])
    ctx.file("BUILD.bazel", '''package(default_visibility = ["//visibility:public"])
# Service units are not compiler inputs and include filenames illegal in Bazel labels.
filegroup(
    name = "sources",
    srcs = glob(["checkout/src/**"], exclude = ["**/.git/**", "**/__pycache__/**", "**/*.pyc", "**/lib/systemd/**", "checkout/src/resources/**"]),
)
exports_files(["checkout/src/.gn"])
filegroup(name = "clang++", srcs = ["checkout/src/third_party/llvm-build/Release+Asserts/bin/clang++"])
filegroup(name = "clang_tools", srcs = glob(["checkout/src/third_party/llvm-build/Release+Asserts/**"]))
''')
    desktop = [
        "usr/lib/x86_64-linux-gnu/lib" + soname for soname in [
            "X11.so.6", "gio-2.0.so.0", "glib-2.0.so.0", "gobject-2.0.so.0",
            "Xcomposite.so.1", "Xdamage.so.1", "Xext.so.6", "Xfixes.so.3",
            "Xrandr.so.2", "Xrender.so.1", "Xtst.so.6", "gbm.so.1", "drm.so.2",
            "xcb.so.1", "gmodule-2.0.so.0", "mount.so.1", "ffi.so.7", "Xi.so.6",
            "wayland-server.so.0", "Xau.so.6", "Xdmcp.so.6", "blkid.so.1",
            "pcre2-8.so.0", "bsd.so.0", "md.so.0",
        ]
    ] + ["lib/x86_64-linux-gnu/lib" + soname for soname in ["z.so.1", "selinux.so.1", "pcre.so.3", "expat.so.1"]]
    # Chromium removes executable sections from its sysroot DSOs. Restore matching
    # runtime packages separately, leaving GN's compile sysroot unchanged.
    _desktop_runtime(ctx)
    # Rust derives RPATHs from the interface artifact: keep both aliases adjacent.
    _run(ctx, [python, "-c", "import os, sys; from pathlib import Path; Path('desktop_link').mkdir(); [os.symlink('../' + sys.argv[1] + '/' + lib, 'desktop_link/' + name) for lib in sys.argv[2:] for name in (Path(lib).name, Path(lib).name.split('.so')[0] + '.so')]", "desktop_runtime"] + desktop)
    build = ctx.read("BUILD.bazel")
    imports = []
    for i, library in enumerate(desktop):
        name = "desktop_" + str(i)
        runtime = "desktop_link/" + library.split("/")[-1]
        interface = runtime.split(".so")[0] + ".so"
        imports.append(":" + name)
        # Defer Rust linkage to the final arguments, after the scoped closure flags.
        build += '\ncc_import(name = "%s", interface_library = "%s", shared_library = "%s", alwayslink = True)\n' % (name, interface, runtime)
    # ELF RUNPATH is not transitive: retain the declared closure as direct dependencies.
    link_files = ["desktop_runtime/" + library for library in desktop]
    linkopts = ["-Wl,--push-state,--no-as-needed"] + ["$(location %s)" % library for library in link_files] + ["-Wl,--pop-state"]
    build += '\ncc_library(name = "desktop", deps = %s, additional_linker_inputs = %s, linkopts = %s)\n' % (repr(imports), repr(link_files), repr(linkopts))
    ctx.file("BUILD.bazel", 'load("@rules_cc//cc:defs.bzl", "cc_import", "cc_library")\n' + build)
    return ctx.repo_metadata(reproducible = True)

_upstream = repository_rule(
    implementation = _upstream_impl,
    attrs = {
        "patches": attr.label_list(allow_files = [".patch"]),
        "libvpx_patch": attr.label(allow_single_file = [".patch"], mandatory = True),
    },
)

def _webrtc_impl(ctx):
    _upstream(
        name = "webrtc_upstream",
        patches = [Label("//crates/pulsebeam-libwebrtc-sys:patches/" + name + ".patch") for name in [
            "core-ios-remove-framework-objc",
            "encoder-preference-order-fallback",
            "runtime-codec-core-budget",
            "recordable-encoded-frame-rtp-identity",
        ]],
        libvpx_patch = Label("//crates/pulsebeam-libwebrtc-sys:patches/runtime-libvpx-workers.patch"),
    )
    nixpkgs_package(
        name = "webrtc_host",
        nix_file = Label("//crates/pulsebeam-libwebrtc-sys:gn/host.nix"),
        repository = "@nixpkgs",
        build_file_content = 'package(default_visibility = ["//visibility:public"])\nfilegroup(name = "tools", srcs = glob(["bin/**"]))\nexports_files(["bin/python3"])\n',
    )
    nixpkgs_package(
        name = "webrtc_test_tools",
        nix_file = Label("//crates/pulsebeam-libwebrtc-sys:gn/test-tools.nix"),
        repository = "@nixpkgs",
        build_file_content = 'package(default_visibility = ["//visibility:public"])\nfilegroup(name = "tools", srcs = glob(["bin/**"]))\nexports_files(["bin/openssl"])\n',
    )
    return ctx.extension_metadata(reproducible = True)

webrtc = module_extension(implementation = _webrtc_impl, os_dependent = True, arch_dependent = True)
