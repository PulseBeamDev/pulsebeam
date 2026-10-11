# PulseBeam documentation

The official PulseBeam documentation site, built with VitePress.

## Preview locally

Use a Linux x86_64 checkout with the [host prerequisites](build.md#host-prerequisites)
installed. Run commands from the repository root; Bazel provisions Node and the
site dependencies. Initial downloads require network access.

```bash
./bazel run //:docs_dev -- --host 127.0.0.1
```

Open the local URL printed by VitePress. The documentation home page should
appear, and edits to site pages should reload in the browser. Stop the development
server with Ctrl+C.

## Validate changes

```bash
./bazel test //docs:check
```

This builds the static site and checks its internal links. `README.md` and
`AGENTS.md` are excluded from site pages, so check their links separately.

The [React quickstart](react/quickstart.md) includes its actual `Room.tsx` and
`main.tsx` sources. `//agents/react:quickstart_types` checks them against the built
public packages, and `//agents/pulsebeam-agent-web:browser` exercises the same
bundled application with two participants and real browser media.
