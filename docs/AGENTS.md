# Documentation authoring

Applies to `docs/**` and every repository `README.md` (via the root `AGENTS.md`). Follow the root and any nearer agent instructions. These rules are sufficient without reading external websites.

## Write for the reader

- Start from a **reader, task, and observable outcome**. Favor the shortest supported path to success over exhaustive background.
- Organize around what developers want to do, not the repository's internal structure. Reveal advanced detail only when needed; link to canonical explanations and references instead of duplicating them.
- Lead with the answer or action. Use clear, descriptive headings, active language, short purposeful paragraphs, consistent technical terms, and meaningful links. Avoid jargon without explanation, hype, filler, and arbitrary readability scores.
- Choose one recommended approach; show alternatives only when they address a real constraint or tradeoff.

## Match the content to the question

- **Tutorial / quickstart:** guide the reader to a reproducible first result with prerequisites, steps, and visible confirmation.
- **How-to:** solve one concrete task with ordered actions and verification.
- **Reference:** state exact APIs, types, defaults, limits, errors, and behavior for quick lookup.
- **Explanation:** build a mental model and explain rationale and tradeoffs.
- **Troubleshooting:** connect symptom → diagnosis → fix → confirmation.
- **README:** orient its actual audience, show minimal verified usage or development commands, and link to deeper material.

These are writing patterns, not mandatory website sections. A README can combine them briefly.

## Choose the clearest form

- Prefer **working code** for integration, **steps** for procedures, **tables** for comparisons, and **flow/sequence/state diagrams** for relationships or behavior. Use a sentence when that is clearer.
- Visuals should explain something prose cannot convey as efficiently—not decorate or repeat it. Keep diagrams focused, accurately labeled, accessible without color, and legible on small screens. Use the renderer's supported formats.
- Keep examples copyable and idiomatic. Include required setup, imports, permissions, cleanup, environment assumptions, and expected results; explicitly mark pseudocode or omitted steps.

## Ground and verify every claim

- Check the owning implementation, API definitions, tests, examples, and build targets before writing. **Code and executable contracts outrank old prose.** Never invent commands, capabilities, guarantees, or metrics.
- Distinguish **released, experimental, and planned** behavior; disclose material limits and failure modes. Protect credentials and keep signing secrets out of client examples.
- Test examples where practical; check links and rendered diagrams. For `docs/**`, run `./bazel test //docs:check` when available; for other READMEs, use the relevant owner checks.
- Review from the reader's perspective: can they find the right step, complete it, recognize success, and recover from a likely failure? Report what was and was not verified.

## Further reading (optional, not required)

- [Diátaxis](https://diataxis.fr/) — content types and reader needs.
- [GitHub documentation content model](https://docs.github.com/en/contributing/style-guide-and-content-model) — practical page design.
- [Google developer style guide](https://developers.google.com/style) and [technical illustrations](https://developers.google.com/tech-writing/two/illustrations) — clarity and visual communication.
- [Claude agent-skill best practices](https://platform.claude.com/docs/en/agents-and-tools/agent-skills/best-practices) — focused instructions and progressive disclosure.
