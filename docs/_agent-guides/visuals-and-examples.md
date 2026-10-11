# Visuals, diagrams, and executable examples

Read when adding or changing diagrams, screenshots, tables, code, CLI snippets, signaling flows, SDK walkthroughs, or tutorials. Follow `docs/AGENTS.md` first.

## Choose the right information shape

Use the simplest form that makes the reader's next action or understanding clear.

| Information | Usually best shown as |
| --- | --- |
| One fact or short warning | Sentence or brief callout |
| Ordered procedure | Numbered steps, code, and observable results |
| Types, fields, defaults, limits, support matrix | Table or compact reference list |
| System boundaries / topology | Small flowchart |
| Messages, handshakes, protocol ordering | Sequence diagram |
| Lifecycle, reconnection, state transitions | State diagram |
| Actual interface controls | Current screenshot with focused annotation |
| How to use an SDK | Idiomatic copyable code, not an ornamental diagram |

Do not add a visual just because a page is long. Before drawing, write a **one-sentence takeaway** explaining what it teaches. If the visual cannot support that takeaway more effectively than a sentence or short code sample, omit it.

## Diagram design

- Prefer a single question per diagram. Start at a useful overview; split details into separate diagrams rather than drawing the entire system.
- Use named actors, explicit directions, descriptive arrow labels, meaningful grouping, and a consistent reading direction.
- Separate **control plane** from **media/data plane**. An arrow for HTTP signaling must not look like the RTP/SRTP or data-channel path.
- Distinguish real network exchange from conceptual relationships. Do not invent protocols, handshakes, timing/order, or delivery semantics to simplify the drawing.
- Label conditional/optional/repeated paths, and show error paths only when they matter to the reader's goal.
- Prefer inline, editable Mermaid for diagrams in VitePress; for other READMEs, use formats actually supported by their renderers. Do not replace editable diagrams with raster screenshots of diagrams.
- Keep labels short but specific. Avoid dense arrows crossing through labels, tiny text, ornate colors, or giant all-in-one architecture maps.
- Place a short takeaway near each diagram; use readable alt text or an equivalent text explanation. Do not rely on color alone. Check light/dark contrast and narrow screens when preview is possible.
- Review each edge against implementation, tests, or protocol contracts. A misleading diagram is worse than no diagram.

## Screenshots, tables, and callouts

- Screenshots should answer a concrete navigation or UI-identification question. Crop or annotate the relevant part, avoid personal data and secrets, and replace images when the interface changes.
- A table helps readers compare independent values. Use concise headers, consistent units, and a limited number of columns; restructure if it is unreadable on mobile.
- Callouts should isolate **actual safety, compatibility, or prerequisite information**, not a second main narrative. Put warnings before the risky action.
- Keep image captions meaningful: state what a reader should notice, not just the image title.

## Verified code snippets

- Use the real public surface for the reader's task: SDK by default for app developers; raw WebRTC / WHIP / WHEP / native signaling when that is specifically what the document teaches.
- Supply language, imports, initialization, prerequisites, environment, required scopes/credentials, method calls, and necessary cleanup. Name any intentionally omitted steps.
- Show the entire important path; avoid inventing helpers such as `setupEverything()` or hidden app infrastructure unless the helper is implemented and linked.
- Prefer runnable, minimal examples over elaborate applications that obscure the API. Do not delete required error handling, authentication, or resource cleanup merely for brevity.
- Keep credentials server-side. Use conspicuous fake placeholders with an explanation of where real values come from.
- State exactly where a command runs (developer host, server, browser console, project root), and which platform/versions it supports.
- Separate shell commands from output. Label expected status, log, or UI state rather than claiming success without a check.
- Every pasted snippet must use a consistent API version and current names, imports, methods, and return semantics. Never mix speculative planned APIs with released usage.
- Validate examples with the owner target, test, or real SDK/server execution when practical. A syntax-highlighted block or docs build is **not** proof of correctness.

## PulseBeam-specific mistakes to avoid

- Treating WHIP/WHEP as if they carry native Catalog/Intent/Mapping messages. Their client capabilities and defaults differ.
- Implying a room is usable from a single successful publish operation without checking that a viewer receives the media.
- Showing or suggesting client-side token-signing secrets instead of server-side issuance and short-lived participant tokens.
- Claiming arbitrary codec/protocol/browser support without an implementation or test boundary.
- Presenting desirable reconnection, transcoding, congestion-control, or tail-latency behavior as a measured or shipped guarantee without evidence.
- Confusing a conceptual SFU forwarding diagram with a precise transport sequence; label which is which.

## Example review questions

1. Can a reader copy the code and supply the documented placeholders without guessing missing pieces?
2. Does the code prove the exact outcome promised by the heading?
3. Does each table value or diagram edge have an identifiable code, test, or protocol source?
4. Does the visual remain legible and meaningful without special colors or a wide desktop monitor?
5. Could a simpler table, diagram, sentence, or code block convey this information better?
