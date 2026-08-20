# Simulation contract

This is the contract for the simulator, not a list of individual scenarios.
The detailed coverage inventory and implementation work are in
[`docs/plans/sim.md`](plans/sim.md).

The simulator is a deterministic distributed-state test. A plan is successful
only when the control plane, every shard projection, every client-visible state
and the user-visible media/data result agree after the system has had time to
settle. A packet count or a non-empty discovery set is not a convergence
oracle.

## Production-shaped default

`LocalNodeSim::new()` is the shared default. Tests must not construct unrelated
latency, loss, shard-count or buggify combinations inline. A test may override
one dimension when that dimension is the subject of the test; the override
must be named and justified by the scenario.

The hardened default that the harness is required to provide is:

| Dimension | Default |
|---|---|
| Node topology | Four worker shards with round-robin participant placement |
| Transport | UDP batch by default; TCP covered by a named transport profile |
| Address family | IPv4 by default; `.with_ipv6()` runs the same production-shaped scenario over IPv6 |
| Link | `LinkProfile::wifi()`: 8–13 ms latency, burst loss, occasional reorder and duplicate, impaired feedback, 100 µs GRO window |
| Capacity | Unlimited unless a BWE test explicitly supplies a bottleneck |
| Randomness | `DEFAULT_SIM_SEED`, with every clock, map, network and fault stream derived from it |
| Buggify | 10‰ deterministic baseline on every plan; named recovery plans may override and force sites |

Four shards are the smallest stable default that exercises owner/destination
and tuple-hash/source-shard differences without requiring every test to invent a
placement. One-shard runs remain valid only as an explicit differential
baseline. `LinkProfile::fiber()` and `LinkProfile::cellular()` are named
special cases for tests whose claim is specifically about those paths.

The harness defaults are intentionally not a one-shard or fault-free baseline:
`LocalNodeSim::new()` uses four shards and a 10‰ buggify rate. The three ignored
cross-shard tests remain visible as known gaps; the shared default does not
make them pass by weakening their oracles.

The permitted profile families are:

- `Production`: the default above, including the baseline buggify schedule.
- `SingleShard`: the same link and seed, explicitly used for a one-versus-many
  differential comparison.
- `Fiber`: clean path for isolating signaling, allocation or state-machine
  behavior from network impairment.
- `Cellular`: the shared high-latency/loss path for mobile-specific claims.
- `Recovery`: `Production` plus deterministic forced named faults and, where
  useful, a higher seeded rate. It must
  force each selected failure point at least once and assert the exact site and
  count; a random percentage alone is not evidence of fault coverage.

`with_bandwidth` belongs to BWE plans, `with_tcp_only` belongs to transport
plans, and explicit shard counts belong to placement-matrix plans. They are not
general-purpose test seasoning. The profile builder should be the only place
that owns the common values.

### Transport and address-family coverage

Production selects the Linux batched UDP transport, including `recvmmsg`,
`sendmmsg`, GRO and GSO. Turmoil cannot provide those kernel APIs, so the
simulator selects `udp_batch_sim.rs`: a separate adapter that uses the shared
batched packet representation and shaper while delegating datagram mechanics to
the simulator socket. Production code above the runtime transport interface has
no simulator branches. A runtime unit test verifies GRO-shaped strided batches,
and an end-to-end plan verifies GSO batching plus reassembled media.

TCP simulation uses the same RFC 4571 framing and connection handoff path as
production. The length field is the full 16-bit range; a valid 5,635-byte first
frame is a regression test. Partial writes retain their unfinished frame so a
later frame cannot corrupt stream alignment. The suite covers UDP and TCP over
IPv4 and IPv6, with `.with_ipv6()` running the complete scenario in the IPv6
network. Turmoil cannot model one dual-stack host with simultaneous IPv4 and
IPv6 sockets, so simultaneous candidate advertisement still needs a production
Linux integration test; the simulator runs the same matrix once per family.

## Signaling contract under test

The workspace wire contract is
[`pulsebeam-proto/proto/signaling.proto`](../pulsebeam-proto/proto/signaling.proto):

- `ClientIntent` is declarative. Repeating the same intent must be idempotent;
  omitting a video request removes that request; `active=false` explicitly
  stops a publication.
- `ServerState` is an ordered reliable-channel diff. Participant and
  publication additions/removals form a roster log. Video bindings are a
  complete group when present. Audio bindings are also a complete group when
  present, including an explicitly empty group.
- A participant is announced because it has a visible media publication. Audio
  publications are in the roster so a client can pin a specific audio track;
  audio must not be smuggled into a video-only publication list.
- A binding is authoritative for the slot it names. The client must not infer
  the current audio speaker or video origin from RTP alone.
- A failed state write must leave the unsent diff pending. A later successful
  write must not skip an addition, removal, or binding replacement.

The supplied `../pulsebeamdev/pulsebeam-js` checkout currently implements a
different, older schema: `StateUpdate` with `seq`, `is_snapshot`,
`tracks_upsert`, `assignments_upsert`, and `request_sync`. The current workspace
proto has `ServerState`, publications, separate video/audio bindings, and no
`request_sync` field. The JS client is useful evidence for desired declarative
client behavior, but it is not a wire-compatible oracle for this checkout until
the generated client and proto are aligned. Browser-state tests must either pin
the matching client revision or add a compatibility test before claiming
coverage.

If the older schema is retained for compatibility, its state reducer also needs
explicit tests for stale snapshots, duplicate IDs, remove/upsert conflicts,
unknown assignment tracks and track-kind mismatches. Its current snapshot path
can replace a newer local sequence without validating that relationship.

The Rust agent currently follows the workspace contract. Its signaling tests
must remain aligned with the same invariants: roster diffs commit only after a
successful channel write, audio shape changes include slot replacement and
emptying, and loudness alone does not generate a signaling update.

## State invariants

Every settled assertion must be expressible as an equality or a monotonicity
claim over these sets:

1. **Control authority.** A live participant has exactly one current
   connection generation, room and owner shard. A retired generation cannot
   create a participant, publication, subscription or route event.
2. **Publication catalog.** Every live video/audio/data publication has one
   origin, one owner and only same-room destinations. A retired publication is
   absent from the catalog, pending queues, pattern tables and future views.
3. **Subscription intent.** Repeating, reordering or retracting an intent does
   not create duplicate group membership. A subscription before publication is
   pending, is consumed once when the publication appears, and disappears once
   when retracted or when its participant leaves.
4. **Shard projection.** Each shard eventually contains exactly the participant,
   runtime, plan and route projection that control assigned to it. Every route
   lookup validates slot and epoch. A view generation cannot publish a remove
   without the corresponding prior install being retired coherently.
5. **Event causality.** A stale, duplicated or late shard event may be ignored
   or replayed safely, but must never resurrect a participant, publication,
   route, assignment or topic from an older generation.
6. **Client roster.** Every observer eventually knows every expected live
   publication in its room and no publication in another room. After removal,
   the exact old set is gone; safety-only checks are insufficient.
7. **Client bindings.** Each video mid has at most one current binding and each
   audio mid has at most one current occupant. Every binding names a live
   publication of the right kind, and an empty binding group is observable.
8. **User-visible result.** Every intended recipient receives the correct
   origin/lane according to that lane’s loss policy. A positive assertion must
   prove traffic moved; a negative assertion must prove the bystander did not
   receive it.

The shard owns its state. The oracle may compare snapshots only after they have
crossed the same value-message boundary as production; it must not read shard
internals through shared mutable state or combine independent counters into a
fake snapshot.

## Required harness assertions

The common harness should provide one reusable `ControlStateOracle` rather than
test-specific maps of discovered IDs. It should model participant, publication,
subscription, route, assignment and topic intent from the plan operations, and
capture value snapshots/events from control and shards for assertions.

At every settle window, and after every injected failure, assert:

- exact live participant and publication sets for every observer;
- exact video and audio bindings, including origin, kind, mid and paused state;
- no duplicate publication, group member, slot binding or data delivery;
- no stale participant, track, assignment, route, topic or epoch after removal;
- all live control objects have a corresponding shard projection;
- all required shard projections have a live control owner;
- pending subscriptions are either still valid and bounded or have been
  consumed/released exactly once;
- established unrelated media stays live while control catches up;
- every intended recipient and every intentional bystander is checked;
- at least one positive frame/message/hop occurred before a success oracle can
  pass.

The existing `assert_room_state_consistent` is retained as a fast safety check,
but it must be supplemented with this positive liveness/convergence oracle. The
current `discovered_tracks`, global `cross_shard_media_frames`, and
presence-only data checks cannot establish these properties.

## Buggify contract

Buggify is a deterministic fault schedule, not a random test decoration. Every
ordinary plan reaches production failure branches at the shared 10‰ baseline;
recovery plans use named forced faults followed by a seeded sweep. A plan that
needs an impairment-free BWE capacity oracle may select `LinkProfile::fiber()`
because feedback loss is then not confused with capacity recovery; that is a
named link-dimension override, not a second general default.

The control/shard recovery matrix must eventually cover, independently and in
coherent combinations:

- endpoint reservation and route allocation for video, audio, realtime data,
  reliable data and reverse routes;
- transaction staging, per-shard view publication, commit and rollback;
- materialization, authentication acknowledgment and TCP adoption commands;
- view-delta delay, duplicate, stale-generation and mailbox-full behavior;
- shard lifecycle-event delivery, controller event-queue pressure and replay;
- signaling-channel write failure and retry of an unsent roster/binding diff;
- participant timeout, graceful delete, reconnect and slot/epoch reuse;
- forward media loss, telemetry loss, reliable reverse acknowledgment loss and
  recovery after each.

Each declared site must be observed and forced in a meta-test. A single
`!fired.is_empty()` assertion is not enough. After each forced fault the oracle
must prove atomicity and eventual convergence; counters only prove that the
fault branch ran.

Fault injection must not turn required control events into silent permanent
loss. If a production lane is intentionally lossy, its replay/latest-wins or
user-visible recovery contract must be explicit and tested. If it is required
for topology, a dropped event must be reconstructed or retried.

## Review gate

Before a new simulator scenario is accepted:

- it uses the shared production profile unless its override is the claim;
- its seed is fixed or promoted from a reproducible sweep;
- its positive oracle proves non-zero traffic and all recipients;
- its negative oracle checks isolation and stale-state absence;
- it runs at least one owner/destination crossing for every applicable lane;
- it checks convergence after both normal lifecycle and the relevant injected
  failures;
- a baseline failure was reproduced before a production fix when the scenario
  is a regression;
- `make test-sim`, `make test`, and `make lint` are run before handoff.
