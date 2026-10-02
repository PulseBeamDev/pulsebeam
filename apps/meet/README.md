# PulseBeam Meet

Static Meet client using only the local `@pulsebeam/react` SDK.

From the repository root, run `./bazel test //apps/meet:check` or
`./bazel build //:meet`; Web and React packages are built automatically.
Production builds use `https://demo.pulsebeam.dev`. `./bazel run //:meet_dev`
defaults to `http://localhost:7070` when `NEXT_PUBLIC_PULSEBEAM_SERVER_URL` is unset.
Serve the export with `./bazel run //apps/meet:preview -- --listen 3000`.

Meet's `check` target covers static checks, and `build` produces the static
export. Real Meet media, playback-policy recovery, reliable topics, and receiver
replacement are exercised by the Web runtime's `browser` target against the
served application. Playback-policy cases cover ordinary gestures both before
and after remote media arrives. Topic-continuity checks observe playback throughout
outbound and inbound chat/reactions, rejecting even a transient same-stream detach
that is restored before the final check. Capture, device replacement, and Agent
ownership are also tested by the React SDK and Web runtime owners.

## Media and topics

Application identities come from admission, and video demand follows mounted
consumers. Reconnect retains logical media consumers, receive policies, local
publications, and topic subscriptions. Replacement receivers restore playback
without remounting or a separate repair control.

Remote audio policy is internal to the shared SDK. Joining, toggling the
microphone or camera, opening settings, sending chat, and leaving are ordinary
user interactions that retry blocked playback. Meet has no audio-unlock button
or repair banner. `NotAllowedError` is retriable; missing or failed audio sinks
remain observable through the Web runtime's diagnostics rather than a bespoke
application repair state.

Meet predeclares its known chat and reaction publisher channels before connecting.
Typed subscriptions and sends keep their existing semantics without a first-send
transport replacement interrupting established media. Reliable topics use
receiver-driven bounded recovery. Their stream identity includes an opaque epoch
from the Agent's first admitted connection, retained across transient replacements,
so a fresh Agent cannot collide with an existing receiver's stream counter.

## Native reconnect limitation

The native DTLS reconnect simulation is excluded from ordinary acceptance under
its [documented exception](../../crates/pulsebeam-simulator/docs/native-dtls-exception.md).
It remains available through the opt-in command in that document. This limitation
does not exclude the browser media and topic recovery contracts.
