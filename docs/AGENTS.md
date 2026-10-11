# PulseBeam documentation agent guide

This file applies to `docs/**` and supplements the repository-root `AGENTS.md`.

## Audience and outcome

Write for application developers who know how to build software but may not know WebRTC. Help them complete real tasks with PulseBeam before teaching its internals. A developer should be able to follow a quickstart or SDK guide without first studying RTP, SDP, ICE, or SFU architecture.

Favor clarity, correctness, and successful execution over comprehensiveness or marketing.

## Accuracy and source of truth

- Inspect the owning implementation, examples, tests, CLI help, API definitions, and build targets before documenting behavior. Use the narrowest relevant sources, following the root agent guide.
- Match actual names, signatures, paths, flags, defaults, error behavior, and supported environments. Never invent APIs or fill gaps with plausible code.
- Distinguish what works now from planned or experimental behavior. State relevant limitations plainly; never imply unverified guarantees.
- Treat existing documentation as context, not authority when it conflicts with working code.
- Show safe usage: keep signing keys and other secrets on the server, and make any example credentials or placeholders explicit.

## Writing style

- Aim for **Flesch–Kincaid grade 8** in explanatory prose: roughly **6–8** for quickstarts and **8–10** for conceptual guides. These are editorial targets, not automated acceptance gates. Do not simplify code, API names, or technical specifications to improve a score.
- Prefer short, active sentences (typically 15–20 words) and paragraphs of 2–4 sentences. One paragraph should convey one idea.
- Use familiar words, concrete verbs, and precise terminology. Define an unfamiliar WebRTC term when it first matters; do not explain concepts the reader does not need yet.
- Explain **what the reader can do**, then **how to do it**, then the underlying **why** when useful. Put advanced mechanics after the simplest working path.
- Avoid hype, filler, unexplained acronyms, unnecessary implementation detail, and claims that something is "easy" or "obvious."
- Be direct but not terse: keep prerequisites, important warnings, and troubleshooting details that prevent failure.

## Page and example design

- Organize task guides around a clear outcome, prerequisites, numbered steps, a way to verify success, and relevant next steps.
- Make examples copyable and representative of the real SDK/API. Provide necessary imports, setup, cleanup, and environment assumptions; mark intentional omissions and placeholders.
- Prefer one complete, minimal working example before presenting options. Do not mix native SDK signaling behavior with WHIP/WHEP behavior.
- Use descriptive headings, correctly tagged code fences, and links that lead to the next action. Keep reference material factual and easy to scan.
- For published pages, use the established VitePress Markdown/frontmatter conventions. Update `docs/.vitepress/config.mts` navigation when a user-facing page is added or renamed.
- Keep `AGENTS.md`, contributor instructions, and other internal guidance out of the published documentation routes.

## Review and validation

1. Verify every changed command, code example, configuration value, and API claim against its owner. Execute examples or a representative test when practical; say explicitly when an example was not exercised.
2. Check terminology and links against neighboring pages. Avoid duplicating information that belongs in a canonical reference.
3. Run `./bazel test //docs:check` from the repository root for documentation changes. Use the root agent guide for broader verification when the change touches product code.
4. Report what changed, what was checked, and any remaining uncertainty.

**Quality bar:** A competent developer should complete the documented task correctly, without needing undocumented context or deep WebRTC knowledge.
