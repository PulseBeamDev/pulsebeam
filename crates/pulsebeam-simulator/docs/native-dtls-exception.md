# Historical native DTLS reconnect acceptance exception

## Current status: retired

The native-agent cutover replaces the former client str0m transport/media
implementation with the shared libwebrtc session path. The replacement controlled
real-SFU lifecycle case has passed actual native media, topics, reconnect, stale
handle rejection, retained owning frames and graceful graph retirement.

The case is selected normally by fast, replay, sweep and scoreboard owners:

```text
tests::native_runtime::native_agents_prove_media_topics_reconnect_and_close
```

There is no case-specific default skip and no opt-in requirement:

```sh
./bazel test //crates/pulsebeam-simulator:fast \
  --test_arg=--filter \
  --test_arg=tests::native_runtime::native_agents_prove_media_topics_reconnect_and_close

./bazel run //:replay -- --seed 4711 \
  --filter tests::native_runtime::native_agents_prove_media_topics_reconnect_and_close
```

Same-seed replay must compare stable semantic outcomes, not cryptographic
ciphertext or timing-adaptive encoded bytes. The scenario prints a bounded
`[native-lifecycle]` report after its media, topic, generation, ownership and
close assertions. Focused controlled acceptance does not waive shared full-suite,
browser or production socket/constructor qualification. Generated bindings are
not mobile runtime certification.

## Historical authorization and cause

During the earlier Bazel migration, the human directed:

> Let's actually ignore that test, by skipping the test with a document here.

That exception excluded only the exact case above. It was reported as skipped,
not passed, and did not waive browser or other native regression obligations.

The old native-agent str0m path could time out after a DTLS decryption error.
The locked `dimpl` 0.7.2 buffered-record reprocessing removed a suffix of epoch-1
records and could abort processing after an authentication/decryption failure,
potentially losing a later Finished record. The origin of the offending packet,
including possible late protected traffic on a reused UDP tuple, was not
conclusively captured.

The cutover does **not** claim to patch that dependency or establish the original
packet's cause. The server RTC engine is unchanged. Reinstatement rests on actual
replacement native-client lifecycle and replay evidence, not suppressed crypto
errors, relaxed media/topic assertions or a simulated decoder.
