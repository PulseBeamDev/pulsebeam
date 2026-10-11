# PulseBeam documentation agent guide

Applies to `docs/**`. Follow the repository-root `AGENTS.md` too; do not duplicate its build or discovery rules.

## Goal

Write for application developers who know how to build software but may not know WebRTC. Help them reach a working outcome without first learning RTP, SDP, ICE, or SFU internals.

Optimize for **time to successful task completion and correct understanding**, not page length, reading-grade scores, or diagram count. Favor the simplest supported path, then reveal advanced detail through links.

## Before writing

1. Identify the reader, their concrete goal, and what success looks like. Find the relevant code, SDK contracts, CLI help, tests, and examples using the root agent guide.
2. Verify what actually works. Source code and executable contracts outrank existing prose. Do not invent signatures, flags, defaults, error codes, support claims, or guarantees.
3. Check whether the behavior is **released, available only on the current branch, experimental, or planned**. Never present an aspiration as a working feature.
4. Choose the page's purpose and the clearest presentation before drafting paragraphs.

## Choose the right page type

Use [Diátaxis](https://diataxis.fr/) as an editorial decision tool, **not** as a mandatory four-section website navigation.

| Reader need | Page type | Write it this way |
| --- | --- | --- |
| "Teach me by doing" | **Tutorial / quickstart** | A guided, end-to-end first success; minimal prerequisites and theory |
| "Help me accomplish X" | **How-to** | Task-specific steps for a developer who already has a working setup |
| "What exactly does X do?" | **Reference** | Complete, precise API/CLI/configuration contracts, defaults, limits, and errors |
| "Why does it work this way?" | **Explanation** | Mental models, architecture, rationale, and tradeoffs |

Keep these purposes distinct. Do not interrupt a procedure with architecture essays or force a narrative into reference material. Link to the appropriate page instead. Organize navigation and titles around developer goals (for example, "Publish video"), not internal subsystems they do not yet know.

## Prefer the clearest representation

**Use structured or visual explanations when they communicate more accurately and with less effort than prose.** Do not default to text walls, but do not add decorative visuals either.

| Question | Preferred format |
| --- | --- |
| How do I integrate this? | Ordered steps with executable code and an observable result |
| What connects to what? | Small Mermaid flowchart or topology diagram |
| What happens, and in what order? | Mermaid sequence diagram |
| What states can this enter? | Mermaid state diagram |
| Which option, value, or limit applies? | Compact comparison or reference table |
| What does this UI look like? | Current, annotated screenshot when necessary |
| What is one simple idea? | One or two clear sentences |

A diagram should **replace** a difficult explanation, not repeat it. Show the minimum necessary actors and relationships; split crowded diagrams into overview and detail. Label protocols and directions accurately, especially HTTP signaling versus WebRTC media/data. Never suggest timing, delivery, or ordering guarantees the implementation does not provide.

Use the site's existing fenced `mermaid` support for source-controlled diagrams. Add a concise textual takeaway and accessible description. Check mobile readability, contrast, and light/dark rendering when preview is available. Prefer a short explanation to an unreadable diagram.

## Write for scanning and action

- Lead with the **outcome** and the recommended path. State prerequisites before commands, expected results after them, and next steps at the end.
- Use descriptive, task-oriented headings; direct, active, second-person language; short paragraphs; and familiar terms. Explain jargon at first use without renaming real technical concepts.
- Cut marketing copy, filler, repetitive summaries, unnecessary alternatives, and implementation detail unrelated to the reader's goal.
- Give one default path. Present alternatives only when a real constraint or tradeoff makes them useful.
- Prefer specific statements over vague claims such as "easy," "fast," "seamless," or "production-ready." Be explicit about limitations and failure modes.
- Keep API names, `RoomId`/`ParticipantId` terminology, track labels, and protocol names consistent with the actual code.

## Make examples trustworthy

- Use the public SDK for the simplest supported application task. Drop to raw WebRTC, WHIP/WHEP, or native signaling only when the page intentionally teaches that integration level.
- Provide runnable, copyable examples with required imports, setup, authentication, cleanup, and environment assumptions. For intentionally partial snippets, name the omissions; label pseudocode explicitly.
- Show how to confirm success, and include the common failure or diagnostic that blocks that task. Troubleshooting should connect **symptom → likely cause → action → verification**.
- Keep signing secrets and privileged credentials on the server; use unmistakable placeholders rather than real keys.
- Do not conflate native SDK signaling with WHIP/WHEP behavior. Document protocol-specific differences and unsupported features precisely.
- Validate examples against the current SDK and server whenever practical. Do not claim a sample was executed unless it was.

## Page patterns

- **Quickstart:** goal → prerequisites → shortest complete path → visible success → common blocker → next task. Aim for a first working video room in roughly ten minutes for the target reader; this is a UX goal, not a claim about current performance.
- **How-to:** use case → prerequisites → steps/code → verification → edge cases and related tasks.
- **Reference:** exact syntax/types → meaning and defaults → constraints/errors → minimal usage example. Completeness and correctness outrank prose simplicity.
- **Explanation:** central insight → small mental model or visual → example → rationale/tradeoffs → links to action and reference.

Do not copy full reference sections into task pages. Link to the canonical source of detail.

## Review and verification

1. **Accuracy:** trace every technical claim, example, number, and diagram edge to the owning implementation or test. Flag anything unverified.
2. **Usability:** follow the page as its intended reader. Can they identify the first action, copy the example, recognize success, and recover from a likely failure? Remove needless conceptual detours.
3. **Presentation:** check links, headings, table layout, Mermaid syntax, accessible text, and navigation. Follow existing VitePress Markdown/frontmatter conventions. Update `docs/.vitepress/config.mts` when adding or renaming public pages; keep agent guidance out of public routes.
4. **Build:** run `./bazel test //docs:check` at the repository root when possible. Preview complex diagrams with `./bazel run //:docs_dev` when visual verification matters. The docs build does **not** prove examples run—test those separately where possible.
5. **Handoff:** report the edits, exact checks performed, untested examples, and material uncertainty. Never imply verification that did not happen.

**Acceptance:** A developer can find the right page, understand the minimum necessary concepts, complete the documented task, and verify the result without relying on undocumented knowledge.

## Editorial references

Consult these only for the relevant problem; keep this guide self-contained:

- [Diátaxis](https://diataxis.fr/) — separate tutorials, how-to guides, reference, and explanation.
- [GitHub documentation content model](https://docs.github.com/en/contributing/style-guide-and-content-model) — practical page design and scannability.
- [Google developer documentation style guide](https://developers.google.com/style) and [technical illustrations](https://developers.google.com/tech-writing/two/illustrations) — clear language and useful visuals.
- [Claude skill authoring best practices](https://platform.claude.com/docs/en/agents-and-tools/agent-skills/best-practices) — concise instructions, appropriate specificity, progressive disclosure, and real-world validation. This is an `AGENTS.md`, not a `SKILL.md`; skill frontmatter conventions do not apply.
