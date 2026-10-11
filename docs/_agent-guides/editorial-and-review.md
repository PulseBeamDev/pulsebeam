# Editorial style, accessibility, and quality checks

Read when polishing technical writing, reviewing new documentation, editing a README, or diagnosing a low-quality docs contribution. Follow `docs/AGENTS.md` first.

## Technical writing rules

- Write for the intended developer's competence, not an imagined beginner in all of computing. Explain PulseBeam/WebRTC-specific concepts but skip universally familiar programming basics.
- Put the answer, action, or takeaway first; supply rationale afterward when needed.
- Prefer direct, active, present-tense sentences. Address the reader as "you" for instructions. Use short paragraphs and one idea per paragraph, but vary sentence length naturally.
- Use precise, stable terminology. Do not casually alternate "room," "session," and "call" when these have distinct meanings in the SDK.
- Use an acronym's expanded form the first time its meaning matters, then the acronym consistently. Avoid repeated redefinitions across a short page.
- Replace jargon with concrete terms when no precision is lost. Do not rename actual APIs, flags, protocols, or fields for readability.
- State preconditions, units, defaults, and environment explicitly. Prefer "The connection closes after X" only if X is authoritative; otherwise state the known condition without invented precision.
- Avoid hype and unsupported absolutes ("zero latency," "works everywhere," "secure by default"). Identify observed, promised, and intended behavior.
- Use sentence-case headings; keep one H1 per Markdown page. Choose task-based headings for procedures and entity-based headings for reference.
- Use second-person imperatives for instructions, not hedged passive constructions. Avoid "simply" or "just" when setup is nontrivial.
- Keep definitions, caveats, and notes close to the claim or step they qualify, not hidden after an unrelated section.
- Be consistent with punctuation, code formatting, units, spelling, and naming within the page. Accuracy and consistency outrank an arbitrary reading-grade score.

## Scannability and discoverability

- The title should communicate the topic or task without relying on navigation context.
- The first paragraph should tell the reader whether the page solves their problem. Avoid long preambles about the company or the documentation structure.
- Each heading should stand alone in a table of contents; replace "Overview," "More," and "Details" where a more precise name is possible.
- Break genuinely sequential work into numbered steps. Use unordered lists for independent items. Do not turn continuous prose into a giant bullet forest.
- Use tables for comparable facts, not as generic page layout. Avoid empty or repetitive table cells.
- Link to the specific local page or heading that solves the next question. Prefer descriptive link text and avoid repeated copies of the same explanation.
- Put a minimal canonical example near the first use of an unfamiliar API, then link to authoritative reference rather than reproducing its entire contract.

## Accessibility and visuals

- A reader should understand the important information without color, animation, screenshot-only text, or the ability to hover.
- Provide alt text that states the information conveyed; for complex diagrams, include a prose explanation of the relationships.
- Use logical heading order and meaningful, short link text. Provide a real text alternative for commands or values embedded in screenshots.
- Avoid large diagrams with microscopic labels or huge wide tables. Test in mobile widths when the renderer is available.
- Keep screenshots current. Remove or redact credentials, account IDs, tokens, emails, and personal data.

## Error and security communication

- Include warnings **before** irreversible or destructive steps, not afterward. Provide a safe rollback or recovery path when available.
- Distinguish development-only convenience from production authentication, network, and deployment requirements.
- Do not place secret signing material in client-side JavaScript, browser consoles, URLs, or screenshots.
- When an operation fails, explain observable symptoms and a useful diagnostic, then an actionable remediation and re-check.
- If the product cannot perform a task, say so before inviting the reader to invest time in an impossible procedure.
- Separate known issues, unsupported behavior, regressions, and roadmap plans; don't blur them into broad promises.

## Editorial review procedure

1. **Find owner truth:** identify the owning package, implementation, API definition, example, test, and applicable nested `AGENTS.md` before composing claims.
2. **Clarify intent:** write down who the page serves, their starting knowledge, the exact reader question, and observable success criterion.
3. **Pick the structure:** choose tutorial / quickstart / how-to / reference / explanation / troubleshooting / README; then choose the minimal code, prose, visuals, and tables needed.
4. **Verify before polishing:** cross-check commands, signatures, security, flags, numbers, protocol flows, versions, and limitations. Fix false content first.
5. **Run the user journey:** follow steps in a representative environment when feasible. If impossible, identify untested assumptions explicitly instead of asserting success.
6. **Review presentation:** verify headings, code fences, diagrams, rendered links, accessible text, relevant warnings, navigation, and mobile legibility when possible.
7. **Use the owning gate:** for `docs/**`, run `./bazel test //docs:check` if available. For README-only updates, prefer owning package checks or markdown/link validation rather than running the docs-site test unnecessarily.
8. **Report evidence:** say which checks actually ran, which examples were not executed, any remaining uncertainty, and whether the changes cover affected companion docs.

## Acceptance rubric

Accept when all material criteria are satisfied:

| Dimension | Must be true |
| --- | --- |
| Correct | No invented contracts, unsupported claims, security violations, or stale examples |
| Actionable | The intended reader knows the prerequisites, first step, expected result, and recovery path |
| Navigable | Searchable headings, useful cross-links, clear audience, no needlessly duplicated content |
| Appropriate | A tutorial teaches by doing; how-to completes a task; reference describes; explanation gives rationale |
| Visual | The chosen representation clarifies, and every diagram relationship is supported |
| Maintainable | Changes follow owner conventions, correct release scope, and relevant checks |
| Transparent | The author reports actual verification and uncertainty without claiming unrun tests |

Do not block a docs contribution on arbitrary sentence length, reading-grade targets, word count, diagrams per page, or minor aesthetic preferences when the reader experience and accuracy are sound.
