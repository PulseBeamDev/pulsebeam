# PulseBeam Meet

Static Meet client using only the local `@pulsebeam/react` SDK.

From the repository root, run `./bazel test //apps/meet:check` or
`./bazel build //:meet`; Web and React packages are built automatically.
Production builds use `https://demo.pulsebeam.dev`. `./bazel run //:meet_dev`
defaults to `http://localhost:7070` when `NEXT_PUBLIC_PULSEBEAM_SERVER_URL` is unset.
Serve the export with `./bazel run //apps/meet:preview -- --listen 3000`.

Meet has no app-level browser tests. Its `check` recipe covers static checks,
and `build` produces the static export. Capture, device replacement, Agent
ownership, and transport behavior are tested by the React SDK and Web runtime
owners rather than coupled to the Meet UI.

## Reliability delivery status

This delivery is committed with a known acceptance blocker, as explicitly
approved by the user. It is not a fully accepted implementation.

Meet now uses MUI controls and shared device settings rather than bespoke UI
primitives. Formatted production TS/TSX/CSS totals 1,185 lines in
`app`, `components`, `hooks`, and `lib`, compared with the 1,694-line baseline
at `0ffe4992`: a 30.05% reduction. Application identities come from admission,
video demand follows mounted consumers, and reliable topics use receiver-driven
bounded recovery. Reliable stream identity includes an opaque epoch from the
Agent's first admitted connection, retained across transient replacements, so a
fresh Agent cannot collide with an existing receiver's stream counter.

Recorded pre-migration checks, not evidence for the current Bazel candidate:

- `pnpm run check` and `pnpm run build` passed.
- Agent-core tests and workspace Clippy passed after the recovery changes.
- The shard replacement regression passed with transport retirement, cancellation
  of a rejected candidate, stale cleanup, and delayed materialization/polling.
- The formerly failing publisher-video and ordered-topic reconnect simulations
  passed. The simulator fast suite subsequently passed 115 of 116 tests.
- A local Chrome proof using fake capture and the public Meet application passed
  at 360px, 768px, and 1280px. It checked overflow and header controls, external
  identity, microphone/camera toggles, local chat acceptance, settings Escape and
  focus restoration, reconnect, and leaving without retaining the token.

Full current-candidate root/browser/slow acceptance is not established. Screen
sharing, reactions, remote pinning, and the complete failure-state UX still need
final interactive acceptance alongside the shared SDK browser gates.

### Blocker: native DTLS reconnect

Reproduce with:

```sh
./bazel test //crates/pulsebeam-simulator:fast \
  --test_arg=--run-skipped --test_arg=--filter \
  --test_arg=tests::native_runtime::native_agents_prove_media_topics_reconnect_and_close
```

The replacement native connection completes ICE and verifies the client
certificate, but stalls before validating Finished. The server records
`decryption failed` while enabling peer encryption, followed by repeated DTLS
flight retransmissions; the test fails with `native agent did not connect`.

The locked dependency is `dimpl` 0.7.2. Its DTLS 1.2
`Engine::enable_peer_encryption` removes buffered epoch-1 records with
`queue_rx.split_off`, then propagates a record parse/decryption error with `?`.
That can discard subsequent valid records, including Finished, while leaving the
server in `AwaitChangeCipherSpec`. Late protected records from a prior transport
can reach a replacement on a reused UDP tuple, but the offending packet's origin
has not been conclusively captured. The existing post-handshake bad-record fix
in this dependency does not cover this buffered handshake transition.

This case is now explicitly excluded from ordinary migration acceptance by a
[human-authorized exception](../../crates/pulsebeam-simulator/docs/native-dtls-exception.md).
The case and its assertions remain available through the opt-in command above.
No dependency patch was vendored and no test oracle was weakened. Follow-up must
repair and regress this dependency boundary, rerun the exact native simulation,
then complete affected owner and root `./bazel test //:test` acceptance, including fresh
browser evidence for the shared reliable-topic wire changes.
