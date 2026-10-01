# Native DTLS reconnect acceptance exception

## Authorization and scope

During the Bazel migration, the human explicitly directed:

> Let's actually ignore that test, by skipping the test with a document here.

The sole excluded case is:

```text
tests::native_runtime::native_agents_prove_media_topics_reconnect_and_close
```

This is an explicit acceptance exception, not a successful test, a flaky-test
classification, or proof that reconnect works. All other committed simulation
cases, seeds, assertions and browser boundaries remain required. The migration
must not claim unqualified complete acceptance without mentioning this exception.

## Why it is excluded

The case was already recorded as a delivery blocker before the Bazel migration
in [Meet's delivery status](../../../apps/meet/README.md#blocker-native-dtls-reconnect).
It also fails under the current Cargo owner workflow. The consolidated Bazel
run reproduced the same native connection timeout after a DTLS decryption error;
this is not established as a migration regression.

The locked `dimpl` 0.7.2 DTLS 1.2 implementation sets peer encryption, removes a
suffix of buffered epoch-1 records using `queue_rx.split_off`, and reparses each
record with `self.parse_packet(&buf)?`. An authentication/decryption error can
abort that iteration and discard later detached records, including a valid
Finished message. The handshake then remains in `AwaitChangeCipherSpec`.
The origin of the offending packet, potentially late protected traffic from a
prior transport on the reused UDP tuple, is not conclusively captured.

The suggested repair is confined to buffered record reprocessing: drop a record
only for the precise crypto decrypt failure, continue with subsequent records,
and propagate unrelated errors. AEAD authentication, replay-window updates and
Finished verification must remain intact. No such patch is applied by this
migration and no test oracle is changed.

## How the exception is enforced

The simulator `BUILD.bazel` supplies an exact `--skip` name to the fast gate,
replay and advisory sweep. The process-isolating runner prints a visible
`[simulation-result] SKIP` record with this document's path. It neither catches
test failures nor alters the compiled test binary. Existing exploratory
`#[ignore]` cases retain their distinct semantics. Slow-case selection is
unchanged because this case is not in the slow namespace.

The source case and all of its assertions remain available. Explicitly opt in:

```sh
./bazel test //crates/pulsebeam-simulator:fast \
  --test_arg=--run-skipped --test_arg=--filter \
  --test_arg=tests::native_runtime::native_agents_prove_media_topics_reconnect_and_close

./bazel run //:replay -- --run-skipped --seed 4711 \
  --filter tests::native_runtime::native_agents_prove_media_topics_reconnect_and_close
```

The test target is expected to fail until the underlying boundary is repaired.
An exact selection without opt-in fails with no runnable cases, rather than
reporting a skipped-only invocation as a successful regression run.

## Reinstatement criteria

1. Capture or faithfully reproduce the invalid buffered record followed by a
   genuine Finished message at the dependency boundary.
2. Add a regression proving the invalid record is discarded without losing
   subsequent valid records, without bypassing crypto validation or state errors.
3. Repair the dependency boundary while preserving the locked compatibility
   contract, then pass the exact native vertical slice and relevant seed replay.
4. Remove `NATIVE_DTLS_EXCEPTION` from the simulator BUILD targets and renew
   affected owner and shared browser acceptance evidence.

Until these criteria are met, retain this document and report the exception
alongside passing gate evidence. No production publication is authorized by it.
