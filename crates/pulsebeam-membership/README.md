# `pulsebeam-membership`

Experimental, control-side gossip membership built on unmodified Chitchat 0.13.0.
For clustered server startup, use the [server configuration guide](../pulsebeam/README.md#experimental-cluster-membership).
This crate does not coordinate rooms, choose placement, authenticate peers, or grant authority.

## Host integration

The host durably allocates a canonical `NodeId` and a positive `u64` generation
before starting gossip. It resolves addresses and supplies an already provisioned
Chitchat `Socket` to `Membership::start`. Production uses Chitchat's UDP socket;
the simulator uses its `ChannelTransport`, which round-trips the same wire codec.
The membership crate does not read files, resolve hostnames, or provision sockets.

`Membership::start` returns the owning service and a cloneable `Handle`. Run the
service with a cancellation token. Cancel and await `Membership::run` before
releasing the host's identity lock. Fatal socket/task errors return to the host;
the server supervises them by shutting down the node. Dropping the service aborts
the protocol task, but explicit cancellation and joining is the supported cleanup path.

## Read local observations

`Handle::snapshot` returns a complete owned snapshot. `Handle::subscribe` returns
a watch receiver. Notifications may coalesce: recover from the complete latest
snapshot, not by treating `changes` as a durable event log. Revisions are local,
monotonic, and reset with the observer process.

An incarnation is `(NodeId, generation_id, gossip_advertise_addr)`. Liveness is
`Known`, `Live`, or `Suspected`, independently of supersession and conflicting
addresses. `supported` distinguishes usable schema/metadata/endpoints from retained
unconfirmed or malformed observations. Only live, non-superseded, non-conflicting
supported records are usable.
The highest observed generation remains current even when suspected. Invalid
metadata/endpoints do not become usable endpoints; valid identity/generation and
address-conflict evidence still prevent silently choosing an older incarnation.
A current-generation address conflict remains sticky while that history is retained;
a new generation resolves it, not removal of one conflicting protocol record.

The wrapper reconciles complete state and liveness under one Chitchat access
boundary every second. It uses one-second gossip, Chitchat's default failure
detector (including its 24-hour dead-node grace), and 15-minute metadata tombstones.
Generation high-water history remains for 48 hours after its last protocol-state
observation, including after protocol GC. Beyond forgotten history or observer
restart there is no everlasting stale-replay protection. `NodeId` is not fencing.

`Handle::replace_metadata` replaces the complete local metadata map. Limits are
16 entries, 64-byte ASCII alphanumeric/`.`/`_`/`-` keys, 1,024-byte values and
8,192 aggregate key/value bytes. Application metadata uses a separate wire prefix
from protocol-owned keys. Replacements and deletions are reflected in snapshots.

Gossip must run only on a trusted, access-restricted network. The cluster ID is
not authentication. Seeds are ordinary peers, not leaders. Shutdown sends no
reliable leave announcement: other observers eventually suspect/remove the node.

## Contributor checks

```sh
./bazel test //crates/pulsebeam-membership:unit_tests //crates/pulsebeam-membership:format //crates/pulsebeam-membership:clippy
./bazel test //crates/pulsebeam-simulator:membership_fast //crates/pulsebeam:cluster_tests
```

The simulator owner reuses its process-wide clock/RNG hooks and isolated runner.
Membership scenarios have no native-media dependency. Replay a reported seed with
`./bazel run //crates/pulsebeam-simulator:membership_replay -- --from-seed 1 --seeds 1 --jobs 1 --show-output`.
Root `//:fast` includes the owner proofs; root `//:check` includes static checks.
The final repository gate remains `./bazel test //:test`.
