# PulseBeam documentation agent guide

Applies directly to `docs/**` and, through the root `AGENTS.md`, to `README.md` files throughout the repository. Follow the root and any nearer `AGENTS.md` too. Use every shared rule below for all documentation; **VitePress-specific instructions apply only to `docs/**`**. This file is self-contained: no external style guide is required.

## Goal

Write for the actual reader. Public docs usually serve application developers who can build software but may not know WebRTC; package READMEs may instead serve SDK integrators, contributors, or maintainers. Help them reach a working outcome without requiring irrelevant PulseBeam or WebRTC internals.

Optimize for **time to successful task completion and correct understanding**, not page length, reading-grade scores, or diagram count. Favor the simplest supported path; link to deeper local documentation only when the reader actually needs it.

When goals conflict, prioritize **technical correctness and safety**, then **task completion**, then **clarity**, then **brevity** and visual polish. Never sacrifice a required limitation or warning merely to make a page shorter.

## Before writing

1. Identify the reader, their concrete goal, and what success looks like. Find the relevant code, SDK contracts, CLI help, tests, and examples using the root agent guide.
2. Verify what actually works. Source code and executable contracts outrank existing prose. Do not invent signatures, flags, defaults, error codes, support claims, or guarantees.
3. Check whether the behavior is **released, available only on the current branch, experimental, or planned**. Never present an aspiration as a working feature.
4. Choose the page's purpose and the clearest presentation before drafting paragraphs.

## Choose the right page type

Choose a page's shape from the reader's question, not from the implementation's module layout. These are four distinct writing modes, **not** mandatory website sections or rigid README templates. A README is often an entry point mixing a short overview, quickstart, development commands, and links to deeper pages.

| Reader need | Page type | Write it this way |
| --- | --- | --- |
| "Teach me by doing" | **Tutorial / quickstart** | A guided, end-to-end first success; minimal prerequisites and theory |
| "Help me accomplish X" | **How-to** | Task-specific steps for a developer who already has a working setup |
| "What exactly does X do?" | **Reference** | Complete, precise API/CLI/configuration contracts, defaults, limits, and errors |
| "Why does it work this way?" | **Explanation** | Mental models, architecture, rationale, and tradeoffs |

Keep these purposes distinct in full documentation pages. Do not interrupt a procedure with architecture essays or force a narrative into reference material. Link to the appropriate page instead. Organize navigation and titles around developer goals (for example, "Publish video"), not internal subsystems they do not yet know. A README may combine a short orientation and onboarding path, then link to specialized pages.

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

A diagram should **replace** a difficult explanation, not repeat it. Before adding one, state the question it answers; if one sentence or a small code sample works as well, use that instead. Show the minimum necessary actors and relationships; split crowded diagrams into overview and detail. Label protocols and directions accurately, especially HTTP signaling versus WebRTC media/data. Never suggest timing, delivery, or ordering guarantees the implementation does not provide.

For `docs/**`, use the site's existing fenced `mermaid` support for source-controlled diagrams. For READMEs elsewhere, use only diagram and asset formats supported by their actual renderers (such as GitHub and package registries); do not assume VitePress extensions work. Add a concise textual takeaway and accessible description. Check mobile readability, contrast, and light/dark rendering when preview is available. Prefer a short explanation to an unreadable diagram.

## Write for scanning and action

- Start with **what the reader will accomplish**, then the recommended path. Put prerequisites **before** commands, expected results **after** them, and a useful next step at the end.
- Use **sentence-case, task-oriented headings** that make sense in search results and a table of contents: prefer "Publish a video track" to "Video overview." A concept page may use a question or named concept instead.
- Address the reader as "you"; use active voice and present tense where accurate. Use short sentences and paragraphs. Give each paragraph one job. Define unfamiliar terms at first use without inventing substitute names for protocol concepts.
- Make instructions unambiguous: say **what to run, where to run it, what to replace, and how to tell whether it worked**. Write commands in copyable fenced blocks with a language tag, and put expected output in separate blocks or prose.
- Use descriptive links ("Deploy PulseBeam" rather than "click here") and link to the most specific local destination. Do not make readers repeatedly follow links to finish a basic task.
- Cut marketing copy, filler, repetitive summaries, unnecessary alternatives, and implementation detail unrelated to the reader's goal. Give one default path; mention alternatives only when a meaningful decision changes.
- Replace vague adjectives ("easy," "fast," "seamless," "production-ready") with testable behavior, exact constraints, or explicitly measured evidence.
- Keep public types, `RoomId`/`ParticipantId` terminology, track labels, and protocol names consistent with the owning code. Do not rename technical concepts merely to simplify prose.
- Use lists for steps, tables for genuine comparisons, and callouts for important warnings—not for decoration. Prefer a focused page over an exhaustive one; move unrelated deep dives to their own pages.

### Concrete writing examples

| Avoid | Prefer |
| --- | --- |
| Heading: "Authentication" followed by an essay | Heading: "Create a participant token" followed by the actual command, required inputs, and where to use the result |
| "Run the server. It should work." | Exact supported command, runtime assumptions, and an observable success signal |
| A paragraph enumerating many message exchanges | A minimal sequence diagram with labeled arrows and one-sentence takeaway |
| "See the API" when a task needs one method | Show the minimal verified method call, then link to the complete API contract |
| Claiming a feature "just works" | State the supported environment, protocol boundary, and known restrictions |

## Make examples trustworthy

- In application-facing documentation, use the public SDK for the simplest supported task. Drop to raw WebRTC, WHIP/WHEP, or native signaling only when intentionally teaching that integration level. In package READMEs, prefer the owning package's canonical integration, build, or test commands.
- Provide runnable, copyable examples with required imports, setup, authentication, cleanup, and environment assumptions. Choose one idiomatic language/platform per task unless the reader needs to compare implementations. For intentionally partial snippets, name the omissions; label pseudocode explicitly.
- Show how to confirm success, and include the common failure or diagnostic that blocks that task. Troubleshooting should connect **symptom → likely cause → action → verification**.
- Keep signing secrets and privileged credentials on the server; use unmistakable placeholders rather than real keys.
- Do not conflate native SDK signaling with WHIP/WHEP behavior. Document protocol-specific differences and unsupported features precisely.
- Keep examples small but complete enough to reproduce. Avoid imaginary helpers, unspecified dependency versions, hidden prerequisites, or placeholders that resemble real credentials.
- Validate examples against the current SDK and server whenever practical. Do not claim a sample was executed unless it was.

## Page patterns

- **Quickstart:** goal → prerequisites → shortest complete path → visible success → common blocker → next task. Aim for a first working video room in roughly ten minutes for the target reader; this is a UX goal, not a claim about current performance.
- **How-to:** use case → prerequisites → steps/code → verification → edge cases and related tasks.
- **Reference:** exact syntax/types → meaning and defaults → constraints/errors → minimal usage example. Completeness and correctness outrank prose simplicity.
- **Explanation:** central insight → small mental model or visual → example → rationale/tradeoffs → links to action and reference.
- **Repository or package README:** what this is / who needs it → minimum supported getting-started or development commands → verification → relevant limitations → links to detailed docs and owner contracts. Keep it a useful entry point, not a duplicate documentation site.

Do not copy full reference sections into task pages. Link to the canonical source of detail.

## Review and verification

1. **Accuracy:** trace every technical claim, example, number, and diagram edge to the owning implementation or test. Check version, release status, exact arguments, required permissions, error behavior, and security boundaries. Flag anything unverified.
2. **Usability:** follow the page as its intended reader. Can they identify the first action, copy the example, recognize success, and recover from a likely failure? Remove needless conceptual detours, repeated information, and visuals that convey nothing new.
3. **Presentation:** check links, heading hierarchy, fenced-code language tags, table layout, diagram syntax, and accessible alt text or equivalent explanation in the intended renderer. Do not communicate meaning with color alone. For `docs/**` only, follow existing VitePress Markdown/frontmatter conventions and update `docs/.vitepress/config.mts` for public page changes; keep agent guidance out of public routes.
4. **Build:** for `docs/**`, run `./bazel test //docs:check` when possible and preview complex diagrams with `./bazel run //:docs_dev` when needed. For other READMEs, use the owning package's relevant checks. Neither a docs build nor a Markdown render proves code examples run—test those separately when practical.
5. **Handoff:** report the edits, exact checks performed, untested examples, and material uncertainty. Never imply verification that did not happen.

**Acceptance:** The intended reader—developer, integrator, contributor, or maintainer—can find the right entry point, understand the minimum necessary concepts, complete the documented task, and verify the result without undocumented knowledge.
