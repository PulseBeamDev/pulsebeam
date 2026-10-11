# PulseBeam documentation agent guide

Applies to `docs/**` and, via the root `AGENTS.md`, to every repository `README.md` and other contributor-facing documentation. Follow the root and any nearer `AGENTS.md`. VitePress-specific instructions apply **only to `docs/**`**.

## Objective and non-negotiables

Write for the actual reader: an application developer who may not know WebRTC, an SDK integrator, or a package maintainer. Optimize for **correct understanding and successful task completion per minute of reading**.

Prioritize **truth and safety → successful task completion → clarity → concision → visual polish**. A short but false example is worse than a longer, reliable one.

- **Verify before writing.** Find the owning code, SDK/protobuf/API declarations, CLI help, tests, examples, manifests, and Bazel targets. Executable contracts outrank stale prose.
- **State availability.** Distinguish shipped/released functionality, working main-branch code, experimental support, and plans. Never present a roadmap feature or performance aspiration as guaranteed behavior.
- **Choose the reader's task.** State who can use the page, what they can accomplish, prerequisites, and an observable result. Prefer the simplest supported path; move optional details to relevant follow-ups.
- **Use the most informative form.** Working code for integration; steps for procedures; tables for comparable facts; flow/sequence/state diagrams for relationships, ordering, and lifecycle; a sentence for a simple claim. A visual should *replace* complexity, not decorate it.
- **Keep claims precise.** Use exact types, flags, names, defaults, return values, platforms, limitations, and error behavior. Do not invent APIs, helper functions, support matrices, timing guarantees, or security properties.
- **Make examples safe and testable.** Include required setup/imports, runtime assumptions, authentication, cleanup, and instructions to verify the result. Name missing steps or pseudocode explicitly. Never put signing secrets in client code.
- **Enable recovery.** Document the common blocker near the affected step: symptom → check → cause → remedy → verification. Disclose unsupported behavior even when no workaround exists.
- **Respect the audience.** Do not require a CRUD-app developer to master SDP, RTP, ICE, or SFU internals before publishing and viewing media. Contributors may need deeper owner-specific detail; avoid generic boilerplate.
- **Write for scanning.** Descriptive task headings, answer-first paragraphs, active voice, consistent technical terms, copyable commands, clear links, and short purposeful explanations. Avoid marketing claims, filler, repetitive text, and arbitrary word-count targets.
- **Keep it maintainable.** Link to a canonical reference rather than duplicating contracts; update affected README, example, diagram, and task pages when behavior changes.

## Choose the page's purpose

These are content patterns, **not required top-level navigation sections**.

| Reader's question | Choose | Essential property |
| --- | --- | --- |
| How can I learn by trying? | Tutorial | Guided, reproducible learning experience with visible milestones |
| How do I try this quickly? | Quickstart | One complete, short path to a useful result |
| How do I accomplish this task? | How-to | Specific goal, ordered steps, verification |
| What exactly does this do? | Reference | Neutral, complete, authoritative contract and constraints |
| Why does this design work this way? | Explanation | Bounded mental model, rationale, tradeoffs |
| Why did this fail? | Troubleshooting | Diagnosable symptom, actionable recovery or known limitation |
| What is this package? | README | Purpose, supported entry point, owner commands, deeper links |

READMEs may combine a short overview, setup, owner-specific commands, and links. Do not require one Diátaxis content type per README.

## Local detailed guidance — read only when relevant

**All mandatory baseline rules are above.** Read the corresponding *local file* for detailed patterns before substantial work of that kind:

- [Page patterns](./_agent-guides/page-patterns.md): tutorials, quickstarts, how-tos, reference, explanations, troubleshooting, READMEs, navigation, maintenance.
- [Visuals and examples](./_agent-guides/visuals-and-examples.md): Mermaid, sequence/state/topology diagrams, screenshots, tables, accessible captions, SDK and CLI sample quality, WebRTC protocol boundaries.
- [Editorial and review](./_agent-guides/editorial-and-review.md): headings, links, voice, terminology, accessibility, security wording, editing workflow, quality rubric.

For a small typo or link fix, the baseline alone normally suffices. Do not read all three references indiscriminately; load the relevant file(s) for the actual work. These files are one level from this entry point and live in the repository so this workflow works without external network access.

## PulseBeam-specific authoring decisions

- Prefer the real public SDK for application tasks; use lower-level WebRTC/WHIP/WHEP or native signaling only when the page explicitly teaches that boundary.
- Distinguish HTTP connection setup from WebRTC transport and native data-channel signaling. WHIP/WHEP clients do **not** use native Catalog/Intent/Mapping messages.
- For a video-room quickstart, verify an actual publisher **and viewer** before claiming the result is a working room. Treat first success in about ten minutes as a usability goal, not a measured current guarantee.
- Keep token signing and secrets server-side. Mark development-only shortcuts clearly.
- Show actual limitations and platform constraints near the user decision they affect.

## Verification and handoff

1. **Accuracy:** trace code, numeric claims, diagram relationships, protocol behavior, and version claims to their owning implementation, definition, or test. Remove invented or unverified assertions.
2. **Reader journey:** is the starting state clear? Can the intended reader run the example, observe success, and recover from a likely failure without undocumented context?
3. **Presentation:** choose useful code/tables/diagrams instead of prose walls, but reject redundant visuals. Check heading hierarchy, links, language tags, readable/mobile diagrams, accessible text, and intended renderer.
4. **Owner checks:** for `docs/**`, follow VitePress Markdown/frontmatter conventions, update `docs/.vitepress/config.mts` for changed public routes, and run `./bazel test //docs:check` when possible. Preview complex visuals via `./bazel run //:docs_dev`. For README-only edits elsewhere, use the owning package's appropriate checks, not a site build by default.
5. **Evidence:** report edits, checks actually run, examples not executed, and material uncertainties. A site build is **not** evidence that snippets run.

**Acceptance:** the intended reader can find the right entry point, complete the stated task, verify it worked, and find accurate answers to the next question without unnecessary prerequisite knowledge.

## External sources — optional reference, not required instructions

These sources inspired the **bundled local rules**. Consult them for nuance, new patterns, or unresolved editorial decisions; no agent needs to browse them to execute this guide.

- [Diátaxis](https://diataxis.fr/) — reader needs, content purposes, and separation of tutorials, how-tos, reference, and explanations.
- [GitHub documentation content model](https://docs.github.com/en/contributing/style-guide-and-content-model) — page structures, quickstarts, procedures, troubleshooting, and navigation.
- [Google developer documentation style guide](https://developers.google.com/style) — precision, consistency, language, headings, links, and formatting.
- [Google technical illustrations](https://developers.google.com/tech-writing/two/illustrations) — information-rich visuals, focused captions, simplicity, and accessibility.
- [Claude skill authoring best practices](https://platform.claude.com/docs/en/agents-and-tools/agent-skills/best-practices) — concise core instructions, proportional detail, conditional local references, and feedback loops. This file is an `AGENTS.md`, **not** a `SKILL.md`; skill-specific metadata/frontmatter rules do not apply.
