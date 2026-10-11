# PulseBeam documentation agent guide

Applies to `docs/**`. Supplement, do not repeat, the repository-root `AGENTS.md`.

## Goal and reader

Write for application developers who can build software but may not know WebRTC. Help them accomplish a task before teaching internals. Quickstarts and SDK guides must not require prior knowledge of RTP, SDP, ICE, or SFU architecture.

Optimize for **correct comprehension and task completion per minute of reading**, not word count, visual count, or a readability score.

## Verify before explaining

- Follow the root guide to find the owning implementation, API/protobuf definition, example, test, CLI help, or Bazel target. Treat code and executable contracts as authoritative over older prose.
- Use exact current names, signatures, endpoints, flags, defaults, errors, and supported environments. Do not invent examples, behavior, guarantees, or roadmap status.
- Distinguish released, experimental, and planned behavior. State material constraints and failure modes without burying them.
- Keep signing keys and secrets server-side. Identify placeholders and environmental assumptions in examples.

## Design information before writing prose

**Prefer the format that conveys the idea fastest and most accurately.** Do not default to paragraphs when a visual or structured presentation would be clearer.

- **Task or integration:** numbered steps, runnable code, and observable results.
- **Component relationships, topology, or data flow:** a small Mermaid flowchart.
- **Request/response, signaling, ordering, or timing:** a Mermaid sequence diagram.
- **Lifecycle or state transitions:** a Mermaid state diagram.
- **Choices, configuration, limits, or tradeoffs:** a compact comparison table.
- **Exact UI appearance:** an up-to-date screenshot when it materially helps.
- **One straightforward point:** a sentence, not a diagram.

Before writing several paragraphs about a system, sequence, or comparison, ask whether a visual could replace them. A useful diagram plus a one- or two-sentence takeaway is often better than a wall of text. Code remains the primary explanation when the reader needs to implement something.

Use the existing VitePress Mermaid support (```mermaid` fences) for editable technical diagrams. Prefer source-controlled diagrams to screenshots of diagrams. Do not add tools or image assets just to decorate a page.

**Visual quality bar:**
- Each visual should answer a specific reader question and replace explanation, not merely duplicate it.
- Show only the elements needed for that question. Split dense diagrams into an overview and optional detailed views.
- Use clear labels, meaningful arrows, and accurate boundaries. Distinguish HTTP signaling from WebRTC media/data paths where relevant; never imply an unverified order or guarantee.
- Put the takeaway near the visual. Provide an accessible text explanation or image alt text; readers must still understand the essential point without the image.
- Check legibility at mobile widths and in light/dark themes when preview is available. Avoid tiny labels, giant tables, and decorative complexity.

## Writing and information architecture

- Aim for **Flesch–Kincaid grade 8** for explanatory prose: **6–8** for quickstarts, **8–10** for concepts. This is a heuristic, not a gate; accuracy and comprehension come first. Do not grade code or protocol specifications.
- Use direct, active language. Prefer sentences around 15–20 words and paragraphs of 2–4 sentences when prose is needed. Avoid filler, hype, redundant restatement, and unexplained jargon.
- Introduce unfamiliar concepts only when needed, define each once, and use consistent SDK/API terminology. Do not rename technical concepts merely to lower the reading grade.
- Put the **result and simplest path first**, then constraints, reasoning, and advanced details. Use headings and links for progressive disclosure instead of copying reference material into every page.
- Give one recommended path by default. Include alternatives only when they change a meaningful decision.

## Page patterns and examples

- **Quickstarts:** objective, prerequisites, shortest complete example, expected result, common blockers, next step.
- **Concept guides:** plain-language takeaway, mental model or visual where useful, concrete example, deeper links.
- **API reference:** exact signatures/fields, semantics, limits, errors, and minimal usage examples. Precision outranks reading grade.
- Keep examples copyable and representative of the actual API, including required setup, imports, cleanup, and assumptions. Mark deliberate omissions. Do not conflate native signaling with WHIP/WHEP.
- Follow the existing VitePress Markdown/frontmatter conventions. Update `docs/.vitepress/config.mts` navigation for new or renamed public pages. Keep agent and contributor instructions out of published routes.

## Review loop

1. **Check accuracy:** trace every claim, example, table value, and diagram relationship to its owning source. Exercise runnable examples when practical; report those not exercised.
2. **Check comprehension:** can a developer scan the page, understand the main idea, and find the next action? Replace avoidable prose blocks with a useful visual or structured format; remove redundant diagrams.
3. **Check rendering:** verify links and Mermaid syntax, and preview complex visuals when possible. Run `./bazel test //docs:check` from the repository root; follow the root guide for wider gates if product code changed.
4. **Report evidence:** summarize edits, checks actually run, and any uncertainty. Improve these instructions based on observed failures in real documentation tasks, not hypothetical edge cases.

**Acceptance:** A competent developer can understand the system and successfully follow the documented task without undocumented context or deep WebRTC knowledge.
