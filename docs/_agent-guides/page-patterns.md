# Documentation page patterns

Read this file when adding, substantially revising, or reviewing a tutorial, how-to guide, concept page, reference, troubleshooting article, or README. Follow `docs/AGENTS.md` first.

## Choose the job of the page

| Reader asks | Primary shape | Success criterion |
| --- | --- | --- |
| "Can I try it and learn?" | Tutorial | Reaches a working result through a guided experience |
| "Can I get started quickly?" | Quickstart | Reaches one meaningful result with little prior reading |
| "How do I do this specific thing?" | How-to | Completes one real task safely |
| "What does this option or API mean?" | Reference | Finds authoritative, complete facts fast |
| "Why does this work like that?" | Explanation | Builds a correct mental model and understands tradeoffs |
| "Why did this fail?" | Troubleshooting | Diagnoses and resolves or identifies a known limitation |
| "What is this package, and how do I use it?" | README | Understands purpose, starts, and finds deeper resources |

Do not impose seven separate website sections. These are **content purposes**, not required navigation labels. A README can combine a brief introduction, a small onboarding path, and pointers to other content. A task page can have a short explanation, but do not let a second purpose displace its primary goal.

## Tutorials: teach through a successful experience

- Pick a plausible end-to-end goal and tell the learner what they will build.
- Provide a prepared, reproducible environment: version, platform, required credentials, dependency setup, and starting state.
- Progress in small **action → observable result** cycles. Tell the reader what should appear after each major step; highlight one thing to notice.
- Make the journey coherent and reliably completable. Use one supported solution; postpone forks, optimizations, and architecture debates.
- Teach unfamiliar concepts as they become necessary, not in an essay before the first action.
- Keep a meaningful final artifact, a success check, and a clear next step. Have someone unfamiliar with the product follow it when practical.

For PulseBeam, a first-room tutorial should provide a real publisher *and viewer*, not stop after successfully sending a WHIP offer. If the working viewer path cannot be demonstrated, narrow the stated outcome instead of claiming a complete room.

## Quickstarts: the shortest independently useful success

- State prerequisites, the result, and the chosen supported path upfront.
- Aim for minutes to first success; do not enforce arbitrary word limits if they would omit required setup or failure recovery.
- Use a minimum complete example and visible confirmation. Move options, theory, and deployment hardening into other pages.
- Include the most probable blocker and a concise remedy; offer concrete next tasks.
- A quickstart is **not** the place to demonstrate every feature.

## How-to guides: solve a real problem

- Phrase the title as an outcome, such as "Publish a screen share" rather than an internal operation name.
- Assume the reader has a basic working system; state the starting condition.
- Give the steps in the order they must occur. Offer optional branches only when supported environments demand them.
- Address operational concerns that change whether the task succeeds: permissions, cleanup, ports, ownership, latency, and failure/retry behavior when relevant.
- End with verification and the relevant follow-on action. Link out for broader explanation.

## Reference: describe actual contracts

Reference is for **lookup**, not a linear tutorial. Mirror the API or configuration surface rather than invented categories.

For each relevant public API, option, command, or message, include as applicable:

- Exact name, spelling, signature, type, transport, and availability.
- Inputs, required versus optional fields, defaults, allowed values, units, and limits.
- Output, side effects, idempotency, ordering, lifecycle, and concurrency behavior *only when guaranteed*.
- Error/return status, failure conditions, timeouts, cleanup, and security restrictions.
- One minimal example illustrating correct use; link to the full workflow for tasks.

Prefer tables for comparable fields, but do not squeeze long behavioral definitions into unreadable cells. Identify generated definitions or owning source; do not hand-maintain a conflicting copy. Distinguish a normative contract from a design goal or aspiration.

## Explanations: make a bounded mental model

- Begin with one reader question: "Why does PulseBeam use HTTP for connection setup?"
- Offer the key insight in plain language, then diagram the relationships or lifecycle if it reduces cognitive load.
- Explain the constraints, rationale, tradeoffs, and consequences; distinguish design judgment from guaranteed implementation.
- Connect concepts to observable effects. Compare alternatives only when they clarify a choice.
- Avoid procedural detours and exhaustive API catalogs. Link to how-to and reference pages.

## Troubleshooting: get unstuck

- Name the **symptom or exact error** so readers can find it by search.
- State applicable versions/platforms and a safe way to reproduce or recognize the issue.
- Provide checks in useful order: probable/cheap causes before rare/invasive ones.
- Connect **symptom → diagnostic check → cause → remediation → confirmation**.
- Separate user misconfiguration from product defect and unsupported behavior. If no workaround exists, say so and provide a tracking or support path if known.
- Place a short remedy near the affected task; use a dedicated article when the diagnostic tree becomes too long.

## Repository and package READMEs

README readers differ by owner: consumers, SDK integrators, contributors, or maintainers. Choose a short entry point that serves that reader.

Suggested order (omit irrelevant sections):

1. One-paragraph purpose, supported use cases, and important status/limitations.
2. Minimal supported install, use, or build command with an observable result.
3. Owner-specific tests, development commands, or integration guidance where appropriate.
4. Relevant platform/security caveats.
5. Links to the authoritative in-repo examples, API surface, design notes, and public docs.

Never copy a full website into a README. Avoid a generic monorepo setup in each crate. Respect local `AGENTS.md`, `BUILD.bazel`, and manifest contracts; do not invent a parallel Cargo/pnpm workflow if Bazel owns the repository.

## Navigation and maintenance

- Create titles based on reader goals and likely search terms; avoid internal jargon until needed.
- Show where the reader starts, what came before, and where to go next. Cross-link related tasks, concepts, and reference surfaces; avoid link-only landing pages.
- Prefer one canonical page per concept/contract, with targeted links rather than copy-paste fragments.
- Keep examples/version claims within their actual release boundary. Do not silently mix main-branch work with a published release.
- When an API, feature, CLI flag, or protocol behavior changes, find and update affected READMEs, quickstarts, references, diagrams, and troubleshooting pages together.
- Do not erase important migration or compatibility information merely because it is no longer the recommended path. Isolate it under clearly labeled version/legacy context.
