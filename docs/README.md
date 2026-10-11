# PulseBeam documentation

The official PulseBeam documentation site, built with VitePress.

```bash
./bazel run //:docs_dev
```

Run `./bazel test //docs:check` to build the static site and validate links before submitting a
change. The [React quickstart](react/quickstart.md) includes its actual `Room.tsx`
and `main.tsx` sources. `//agents/react:quickstart_types` checks them against the
built public packages, and `//agents/pulsebeam-agent-web:browser` exercises the
same bundled application with two participants and real browser media.
