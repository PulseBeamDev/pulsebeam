# Contributor checkpoint: ordered recovery across native reconnects

**Status: blocked, not qualified.** The native cutover's ordered-recovery replay
still fails. This checkpoint preserves a partial Core repair and its component
regressions; it does not establish end-to-end ordered recovery or completion of
the native MVP. The selected scope is to record the blocker, not extend the
catalog contract or introduce a cursor-retention policy.

## Partial repair

`Topics::unbind(TransportReplaced)` preserves each ordered delivery frontier while
clearing buffered payloads, latest/lossy state, and the existing transport-owned
state. Explicit session close and unsubscription still discard the cursor.
Catalog pruning is unchanged.

The focused unit test restores the known publication catalog directly and checks
that the replacement transport probes from sequence 4 with `live=false`. It also
checks that a late close from the retired generation cannot fence the replacement.
The separate guard checks current close, catalog-removal, and unsubscribe behavior.
These are component tests, not a substitute for signaling bootstrap or a real
native transport witness.

No native reconnect, ICE, SCTP, retry, queue, deadline, history-capacity, or media
algorithm behavior is changed. Temporary cursor-reset warning instrumentation
used for the investigation was removed before this checkpoint.

## Reproduce the known failure

This section records the **historical checkpoint pair**, not necessarily the
latest candidate pin. For this exact reproduction, use a consumer checkout at
`bd4d563e929e6091ff90d323b6bd2e51ea6af828` and the matching archives below; do not
substitute a newer SDK into that older pin.

Run from that consumer repository root with the [Bazel prerequisites](../../docs/build.md#host-prerequisites) and the
local SDK candidate archives. The exact producer revision is
`f91f2fb16a5fe45e152f398e70dd9961f8c26dc4`; the consumer base for the investigation
is `eb4dfc0129ea8deea0196aab8ee2191a4a95f490`. The producer must have the matching
archives available locally; that checkpoint's `tools/webrtc-candidate.json`
records their identities:

| Archive | SHA-256 |
| --- | --- |
| `webrtc-sys-source-linux-x86_64.tar.gz` | `a84f75fcc6707323134585404bac0423bb8c546f786282278cc72b2d191a700c` |
| `webrtc-core-linux-x86_64.tar.gz` | `d838b78261f93265e5d651ae9aacffe4b03821d19c8732fdbba1e5ae4f5595a8` |

Set `SYS_WORKTREE` to the absolute producer checkout path. The investigation used
`/home/lukas/wt/pulsebeam-libwebrtc-sys/native-libwebrtc-mvp/integrated-cutover`.
Its original source archive was preserved before subsequent candidate updates:

```sh
SOURCE_ARCHIVE="$SYS_WORKTREE/dist/webrtc-sys-source-f91f2fb16a5fe45e152f398e70dd9961f8c26dc4.tar.gz"
NATIVE_ARCHIVE="$SYS_WORKTREE/dist/webrtc-core-d838b78261f93265e5d651ae9aacffe4b03821d19c8732fdbba1e5ae4f5595a8.tar.gz"
```

If using another checkout, set `SOURCE_ARCHIVE` and `NATIVE_ARCHIVE` to archives
with the respective checksums above.

```sh
./bazel run \
  --repo_env=PULSEBEAM_WEBRTC_SOURCE_ARCHIVE="$SOURCE_ARCHIVE" \
  --repo_env=PULSEBEAM_WEBRTC_NATIVE_ARCHIVE="$NATIVE_ARCHIVE" \
  //:replay -- --exact \
  --filter tests::data_channel::ordered_recovery_reports_retention_gaps_without_replaying_late_history \
  --show-output --artifacts-dir /tmp/native-cutover-ordered-recovery-checkpoint
```

**Expected current outcome: failure**, not a passing acceptance gate. The failing
subscriber has only sequences `[0, 1, 2, 3]`, with no recovery gap, instead of 260
delivered messages. Its connection is `Connected`, generation is 19, and the
surviving data-track cursor has advanced to 304.

For the passing component and hygiene gates, use the same archive flags with:

```sh
./bazel test \
  --repo_env=PULSEBEAM_WEBRTC_SOURCE_ARCHIVE="$SOURCE_ARCHIVE" \
  --repo_env=PULSEBEAM_WEBRTC_NATIVE_ARCHIVE="$NATIVE_ARCHIVE" \
  //agents/pulsebeam-agent-core:unit_tests \
  //agents/pulsebeam-agent-core:format \
  //agents/pulsebeam-agent-core:clippy \
  //crates/pulsebeam-simulator:format \
  //crates/pulsebeam-simulator:clippy --test_output=errors
```

## Measured remaining reset

The diagnostic replay isolates catalog pruning, not a channel-close or another
unbind reset, in this cut:

| Simulated time | Subscriber | Generation | Pruned cursor |
| --- | --- | --- | --- |
| 33.904 s | `192.168.1.3` | 19 | `Some(4)` |
| 33.908 s | `192.168.1.4` | 20 | `Some(4)` |

Both refer to `dat_0YA0F0DFF4Y4JF4MGQV65P63MYG`. The same identity later reappears;
the final cursor is 304 without delivery of the retained history. Preserving the
cursor during transport unbind is therefore necessary for the component case but
insufficient for the native replay.

Retained local evidence, not checked-in fixtures:

- `verifier:74ee6739-7a0f-4687-ad17-1cd7085cc29a`: all five listed
  Core/hygiene targets pass; the exact native replay fails. Log:
  `/tmp/pi-verifier-zzOKcq/output.log`.
- `verifier:d34f65ea-088f-4c8a-8d61-9069edf4d678`: the same gates pass with
  temporary diagnostics; the replay fails. Log:
  `/tmp/pi-verifier-WUmpln/output.log`, cursor markers at lines 10543 and 10546.
- Diagnostic replay bundle:
  `/tmp/native-cutover-topic-cursor-reset-cut/run-i81_t41_/case-0000`.
  Its `trace.log` retains the measured cut; it cannot regenerate the removed
  diagnostic markers without restoring that instrumentation.

## Contract gap and next boundary

Core initializes a new generation's observed catalog to empty. Every applied
signaling state reconciles topic bindings against the currently visible catalog,
and absent IDs are pruned. Server catalog planning uses the current downstream
publication view plus the receiver's local data publications.

Opening a subscriber data channel queues `TrackSubscriptionAdded` control work.
The controller resolves existing matches and handles later matching publications
through individual lifecycle effects. There is no explicit acknowledgement that
the initial subscription metadata seed has been fully applied to downstream
state. A complete point-in-time catalog snapshot is not an acknowledgement of
that subscription seed, nor an authoritative publication-retirement record.

Catalog revisions order snapshots and deltas. Applied intent revisions and local
data-channel-open checks do not establish remote catalog completeness. The
measured trace proves absence followed by reappearance of the same identity; it
does not dump the server's exact publication input at the initial snapshot, so a
more specific server-side activation cause remains unproven.

The relevant owners are:

- `agents/pulsebeam-agent-core/src/topic.rs`: transport fencing and catalog pruning.
- `agents/pulsebeam-agent-core/src/agent.rs`: generation activation and signaling
  state reconciliation.
- `agents/pulsebeam-agent-core/src/signaling_v1.rs`: observed catalog membership.
- `crates/pulsebeam/src/participant/core.rs`: catalog input and topic-channel opens.
- `crates/pulsebeam/src/participant/signaling_v1.rs`: snapshot/delta planning.
- `crates/pulsebeam/src/shard/events.rs` and
  `crates/pulsebeam/src/control/controller.rs`: subscription
  control work and lifecycle reconciliation.

A later strict fix must distinguish temporary discovery from authoritative
retirement while providing bounded cleanup. It must cover an initially empty
catalog followed by same-ID reappearance, plus genuine retirement, close, unsubscribe, and
stale-generation cases, then rerun native recovery and the broader qualification
boundaries. Arbitrary settling delays, unbounded historical cursor retention, or
silent cache eviction are not adopted here. Full simulator/shared/browser/
production qualification and final MVP acceptance remain unfinished.
