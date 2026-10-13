# Documentation authoring

Applies to `docs/**` and every repository `README.md` (via the root `AGENTS.md`). Follow the root and any nearer agent instructions. These rules are sufficient without reading external websites.

## Write for the reader

- Public product documentation serves people using, integrating, or operating the software. Do not assume a repository checkout or knowledge of its internals. Keep contributor setup and internal tooling in clearly labeled contributor documentation; READMEs serve their actual audience.
- Start from a **reader, task, and observable outcome**. Favor the shortest supported path to success over exhaustive background.
- Organize around what developers want to do, not the repository's internal structure. Recommend the supported high-level API or SDK when it fits the task; keep protocol-level and operational use cases on distinct, clearly labeled paths.
- Lead with the answer or action. Use clear, descriptive headings, active language, short purposeful paragraphs, consistent technical terms, and meaningful links. Avoid jargon without explanation, hype, filler, and arbitrary readability scores.
- Choose one recommended approach; show alternatives only when they address a real constraint or tradeoff.

## Get to the first useful result

- Entry pages should briefly explain what the software helps the reader accomplish and provide an obvious next action near the top. Do not require architecture reading before getting started.
- An integration quickstart should take the reader through obtaining the software, required configuration, minimal working code, and visible confirmation in their own application. A repository demo can supplement this path, not substitute for it.
- State prerequisites before they are needed. Keep required setup, security boundaries, and blockers visible; reserve collapsible sections for lengthy source and optional detail.
- Introduce concepts only when they help the next action. Explain what the reader must configure or own, not how every internal layer works. Link advanced configuration, implementation detail, and rationale from the relevant step.
- Keep the main task free of distractions: show the shortest idiomatic command, retain only essential security caveats, and move platform quirks, version rationale, optional flags, and detailed limits to troubleshooting or reference.
- When an integration crosses environments, **identify the owner before naming a backend or client**: product-provided server, developer-owned application backend, and developer-owned browser frontend. A small three-node flow diagram can clarify ownership and token/media direction once; keep subsequent instructions short. Keep signing secrets and privileged SDKs on the application backend.
- Keep tutorial handoffs coherent. Preserve configuration, credentials, versions, and running processes when continuing a task; explicitly announce any reset or separate example.

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
- For beginner SDK quickstarts, show one small, idiomatic example that completes the primary task. Avoid bundling optional controls, diagnostic UI, and advanced lifecycle management into the first snippet; link those from focused guides instead.
- Visuals should explain something prose cannot convey as efficiently, not decorate or repeat it. Keep diagrams focused, accurately labeled, accessible without color, and legible on small screens. Use the renderer's supported formats.
- Keep examples copyable and idiomatic. Include required setup, imports, permissions, cleanup, environment assumptions, and expected results; explicitly mark pseudocode or omitted steps.
- Handle user-triggered async failures where they occur and show actionable errors in the UI. Do not use console logging as the only error handling in beginner examples.

## Ground and verify every claim

- Check the owning implementation, API definitions, tests, examples, and build targets before writing. **Code and executable contracts outrank old prose.** Never invent commands, capabilities, guarantees, or metrics.
- Verify that the documented distribution is available to the intended reader and that the components and versions used together are compatible. If a consumer path is unavailable, state the limitation rather than presenting internal setup as normal onboarding.
- Distinguish **released, experimental, and planned** behavior; disclose material limits and failure modes. Protect credentials and keep signing secrets out of client examples.
- Check the actual documented examples from the reader's stated starting point, without relying on unstated repository fixtures or preconfigured environments. Check links and rendered diagrams. For `docs/**`, run `./bazel test //docs:check` when available; for other READMEs, use the relevant owner checks.
- Inspect the rendered entry path at desktop and narrow widths. Can a new reader find the recommended step, complete it, recognize success, and recover from a likely failure? Report what was and was not verified.

## Further reading (optional, not required)

- [Diátaxis](https://diataxis.fr/): content types and reader needs.
- [GitHub documentation content model](https://docs.github.com/en/contributing/style-guide-and-content-model): practical page design.
- [Google developer style guide](https://developers.google.com/style) and [technical illustrations](https://developers.google.com/tech-writing/two/illustrations): clarity and visual communication.
