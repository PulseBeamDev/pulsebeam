# Simulation contract

This is the contract for the simulator, not a list of individual scenarios.
The detailed coverage inventory and implementation work are in
the authored plans in `src/tests/`.

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

The shared harness default is:

| Dimension | Default |
|---|---|
| Node topology | Four worker shards with round-robin participant placement |
| Transport | UDP batch by default; TCP covered by a named transport profile |
| Address family | IPv4 by default; `.with_ipv6()` runs the same production-shaped scenario over IPv6 |
| Link | `LinkProfile::wifi()`: 8–13 ms latency, burst loss, occasional reorder and duplicate, impaired feedback, 100 µs GRO window |
| Capacity | Unlimited unless a BWE test explicitly supplies a bottleneck |
| Randomness | `DEFAULT_SIM_SEED`, with every clock, map, network and fault stream derived from it |
| Internal faults | Explicit named simulation hooks; the generic buggify primitive has no production call sites |

Four shards are the smallest stable default that exercises owner/destination
and tuple-hash/source-shard differences without requiring every test to invent a
placement. One-shard runs remain valid only as an explicit differential
baseline. `LinkProfile::fiber()` and `LinkProfile::cellular()` are named
special cases for tests whose claim is specifically about those paths.

The harness defaults exercise multiple shards and seeded network impairment.
Internal faults require explicit named controls; ordinary plans do not implicitly
exercise them.
The [historical native-str0m DTLS exception](native-dtls-exception.md) is retired.
Native lifecycle, cross-shard media and data proof remain required in ordinary
selection. Ignored subprocess fixtures run through their parent assertions and
are not product exceptions.

The permitted profile families are:

- `Production`: the default above, including seeded network impairment.
- `SingleShard`: the same link and seed, explicitly used for a one-versus-many
  differential comparison.
- `Fiber`: clean path for isolating signaling, allocation or state-machine
  behavior from network impairment.
- `Cellular`: the shared high-latency/loss path for mobile-specific claims.
- `Recovery`: `Production` plus deterministic forced named faults. It must
  force each selected failure point at least once and assert the exact site and
  count; a random percentage alone is not evidence of fault coverage.

`with_bandwidth` belongs to BWE plans, `with_tcp_only` belongs to transport
plans, and explicit shard counts belong to placement-matrix plans. They are not
general-purpose test seasoning. The profile builder should be the only place
that owns the common values.

### Transport and address-family coverage

Production uses `pulsebeam-udp` for Linux batched UDP I/O, including `recvmmsg`,
`sendmmsg`, GRO and GSO. Simulation uses the same borrowed-datagram and exact
send-prefix API with a host-owned device adapter in `pulsebeam-runtime`. The host
owns reuseport steering and network shaping; the I/O crate owns bounded receive
storage and logical GSO/GRO boundaries. I/O conformance tests verify those
boundaries, host tests verify steering and handoff metadata, and end-to-end plans
verify GSO batching plus reassembled media.

TCP simulation uses the same RFC 4571 framing and connection handoff path as
production. The length field is the full 16-bit range; a valid 5,635-byte first
frame is a regression test. Partial writes retain their unfinished frame so a
later frame cannot corrupt stream alignment. The suite covers UDP and TCP over
IPv4 and IPv6, with `.with_ipv6()` running the complete scenario in the IPv6
network. Turmoil cannot model one dual-stack host with simultaneous IPv4 and
IPv6 sockets, so simultaneous candidate advertisement still needs a production
Linux integration test; the simulator runs the same matrix once per family.

## Signaling contract under test

The native wire definition is
[`pulsebeam-proto/proto/state.proto`](../../pulsebeam-proto/proto/state.proto).
It replaces the former Catalog/Intent/Mapping protocol:

- Desired records carry complete per-resource values. Omission does not delete
  a resource; explicit absence permanently ends that owned lifetime.
- Confirmed records distinguish accepted state from evaluation of a newer
  target. An old completion cannot settle newer desire or revive a retired owner.
- Discovery is incremental. `initial_complete` describes initial enumeration,
  not an atomic snapshot, convergence or proof that an undiscovered source ended.
- A source incarnation has an immutable identity, kind, name and owner. A name
  change creates a new publication lifetime. Visibility is not ownership.
- Failed writes preserve current unsent state; stale queued work cannot replace
  a newer accepted source or bypass retirement.
- Native HTTP bootstrap selects `pulsebeam.media.v1`. Each channel-0 message is
  one bounded raw protobuf `ClientStateBatch` or `ServerStateBatch`, without a
  prefix or compression. WHIP/WHEP retain their HTTP/SDP lifecycle and never
  require these messages.

The shared Core lifecycle refactor targets permanently source-bound physical
receivers, not a receiver pool or Top-N audio selection. Compatible consumers
may share work for the same exact source. A replacement incarnation requires a
fresh physical realization, while Core owns logical subscription intent and
explicit discontinuities. Tests must prove correct source/content and stale-work
fencing, not require one native receiver to survive switching between sources.
Data transport and recovery remain provisional pending their separate design.

## State invariants

Production owners enforce structural invariants locally. Simulation proves the
corresponding public lifecycle and recipient-output contracts, not these private
sets through exported snapshots:

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
7. **Client bindings.** Each physical receiver is dedicated to one exact source
   incarnation for its lifetime. Retirement cannot detach a replacement receiver
   or expose queued old-source media as a new source. Logical subscription intent
   and physical realization have separate lifetimes; discovery absence alone
   does not terminate an owned source.
8. **User-visible result.** Every intended recipient receives the correct
   origin/lane according to that lane’s loss policy. A positive assertion must
   prove traffic moved; a negative assertion must prove the bystander did not
   receive it.

The shard owns its state. Keep its slot, epoch, route and projection invariants
inline with production ownership transitions. Exporting private values through
messages or SDK wrappers does not make them public correctness observations.

## Required harness assertions

The common harness uses `ApplicationStateOracle` for intent derived from plan
operations, not server-selected expectations. It binds opaque active admission
IDs through public HTTP/native facts and compares SDK catalog, bindings and
recipient output. It does not read control/shard state, wire acknowledgements,
private route counters or server allocation estimates.

Settled and post-recovery checkpoints assert:

- exact live participant and publication sets for every observer;
- exact video and audio assignments, including origin and kind, independently of forwarding state;
- no duplicate publication, group member, slot binding or data delivery;
- no stale participant, track, assignment or delivered topic after removal;
- pending intent remains legal while bounded; repaired intent converges;
- retired producer and receiver generations cannot credit replacement output;
- established unrelated media stays live while control catches up;
- every intended recipient and every intentional bystander is checked;
- positive actual decoded frames or exact messages occur before a delivery
  assertion passes; source-age claims additionally cover the complete window.

`assert_room_state_consistent` remains a fast safety check. Settled checkpoints
also run the independent convergence oracle. Discovery alone, global frame
counters and presence-only data checks cannot replace exact ownership/intent
checks or positive per-recipient origin evidence.

## Internal fault coverage

`StallController`, `SendToWrongShard` and `FailNextMaterialization` invoke real
simulation-only production seams. Named scenarios in
`src/tests/connectivity.rs` check their consumption and media recovery. These are
authored regression cases, not exhaustive runtime-schedule exploration.
The generic runtime `buggify!` primitive has no production call sites and does
not establish baseline or exhaustive fault coverage.

These hooks do not imply coverage of every allocator, transaction stage,
mailbox-pressure condition, signaling write or shard lifecycle failure. Network
impairment and explicit connection lifecycle operations have their own witnesses.
A plan that needs an impairment-free BWE capacity oracle may select
`LinkProfile::fiber()` because feedback loss is then not confused with capacity
recovery; that is a named link-dimension override, not a second general default.

Every declared fault site needs a reachable forced witness and an exact
consumption count. A single `!fired.is_empty()` assertion is not enough. After
each forced fault the oracle must prove atomicity, eventual convergence and
positive fresh delivery; counters only prove that the fault branch ran.

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
- verification follows the [contributor testing contract](../../../docs/build.md#testing-contract).
