# Cluster topology and ownership

**Status:** proposed cluster architecture. The single-node placement and
compiled forwarding behavior described below exist; cross-node room
coordination, membership, snapshot exchange, and media transport are not yet
implemented. This document defines their intended boundaries, not an
availability guarantee.

The goal is to let one logical room span nodes while keeping WebRTC
connections and packet processing locally owned. Clients reconstruct desired
state after failures; the cluster reconstructs only the derived room view and
forwarding topology.

## Terminology

| Term | Scope and meaning |
| --- | --- |
| **Room** (`RoomId`) | Logical room spanning zero or more nodes. Identity, not an execution or leadership unit. |
| **RoomShard** | The portion of one room hosted by **one node**. At most one per `(RoomId, NodeId)`. Contains that room's node-local partitions and authoritative live connection facts. Node-local membership and partition occupancy use `control::room::RoomShard` today; cross-node synchronization is proposed. |
| **RoomPartition** | Node-local admission group of participants assigned to one execution `Shard` (tracked by `control::room::RoomPartition`). Many partitions can belong to one RoomShard; several partitions may map to the same execution shard. |
| **Shard** (`ShardId`) | Node-local dataplane state owner and packet executor. Hosts partitions from many rooms. A worker may execute multiple shards; workers are *not* owners. |
| **Node** (`NodeId`) | Process owning its own connections, room shards, and execution shards. |
| **RoomCoordinator** | Rendezvous-selected node maintaining an eventually consistent *derived* view of a room's RoomShards and coordinating cross-node topology. It does not own remote connections or their desired state. |
| **Publication** | One live room-scoped track with a publisher and `TrackId`; applies to audio, video, and data. A data-channel **topic** is a label used by subscription selectors, not a cluster routing authority or another RoomShard. |

The word *shard* without a qualifier means the execution `Shard` in
[the dataplane contract](shard-owned-dataplane.md). Never use *room shard* for
participants sharing one execution shard; that is a RoomPartition. Do not use
*partition* as a synonym for a node-owned RoomShard.

```text
Room (logical identity)
  ├── RoomShard (owned by Node A)
  │     ├── RoomPartition 0 -> execution Shard A/2
  │     └── RoomPartition 1 -> execution Shard A/5
  └── RoomShard (owned by Node B)
        └── RoomPartition 0 -> execution Shard B/1

RoomCoordinator (rendezvous-selected; may be A, B, or another node)
    derives room-wide topology from the RoomShards.
```

## Authority: desired state versus live facts

| Owner | Authoritative for | Must not own |
| --- | --- | --- |
| **Client** | Desired publishing, receiving, and media preferences; reapplies them on reconnection. | Server transport state, current route placements, or other participants' connections. |
| **Connection-owning node** | Accepted connection incarnation, actual live publications, subscription intent received, transport and local RoomPartition placement. | Connections on another node. |
| **RoomCoordinator** | Its derived room-wide view, reconciliation, and cross-node forwarding decisions based on owner reports. | Mutable connection state on participant-owning nodes; a permanent room session. |
| **Execution Shard** | Its participant, transport, route, packet and fanout runtime. | Global discovery, subscriptions as selectors, coordinator election. |

A RoomShard is authoritative for **actual** connections and publications on its
node, not for a client's desired state. The RoomCoordinator assembles owner
reports. It must not mutate or impersonate another RoomShard to make its own
eventually consistent view appear strongly consistent.

## Local partition placement (implemented)

The controller places each joining participant in a RoomPartition. A partition
has an explicit index, its owning execution `ShardId`, and live occupancy.
The default capacity target is 16 participants per partition (configurable
through `NodeBuilder::room_partition_size`). Placement chooses the first
partition with space, or opens the next partition. A replacement of a live
participant keeps its partition index; departures release capacity without
renumbering the surviving participants' partitions.

For the default hashed policy, the **local** router selects the execution
shard using `(RoomId, partition_index)`. The simulation policy can choose
`partition_index % local_shard_count` instead. Hash collisions are valid:
multiple RoomPartitions may live on the same execution Shard.

Partition indices are node-local and valid only for their RoomShard lifetime.
Empty indices may be reused; all partition state is dropped when the last
participant leaves the local room. They are not globally unique IDs, durable
partitions, or distributed fencing tokens. The local router's random per-process
hash seed is **not** the cluster's rendezvous hash.

A prepared connection retains its selected partition index. Before commit the
controller checks that this index is still the appropriate placement, rejecting
a stale preparation rather than installing it into a retired local partition.
The existing single-node controller serializes materializations. Future
concurrent/multi-node admissions must preserve this invariant.

## Proposed cross-node topology

1. A node owns each accepted connection and creates/updates its own RoomShard
   projection. One node may host multiple RoomPartitions of the same room.
2. Participating nodes send versioned, full RoomShard snapshots to the
   rendezvous-selected RoomCoordinator. Incremental changes may optimize the
   steady state but cannot be required for recovery.
3. The RoomCoordinator reconciles publications, subscribers and their *owner
   nodes* into the room-wide view. It computes the needed cross-node fanout,
   not arbitrary multi-hop publication discovery.
4. A node's controller translates its authorized forwarding assignments into
   [compiled TrackPlans](routing.md) on its execution shards. The dataplane
   never calls the coordinator on a packet path.
5. Media travels directly from the publishing node to each destination node
   that needs it, then fans out to local subscribers. There is no
   forward-a-remote-arrival-again relay chain.

Publication identity and discovery remain room scoped. Selectors (including
data-topic labels) are resolved by control; media packets use compiled route
addresses, not publication IDs or topic names. The existing
[route envelope and epoch rules](routing.md#addresses-and-stale-route-safety)
remain the node-local forwarding contract. Cross-node routes additionally
need the destination node and a restart-safe incarnation/freshness check;
a `NodeRouteAddress` alone cannot locate an endpoint on another node.

## Coordinator selection and consistency limits

Rendezvous hashing deterministically names a **preferred** coordinator for
`RoomId` from a common membership view. This is separate from the
process-local hashing that places RoomPartitions. Membership views can differ
during failures or partitions: rendezvous hashing does **not** grant an
exclusive lease or guarantee exactly one active coordinator.

Every RoomShard report must identify its node incarnation and monotonically
increasing fragment revision. The receiver accepts only fresh reports from
the same incarnation; a new incarnation replaces old fragment state through
a complete snapshot, never by replaying arbitrary missed deltas. The
coordinator is reconstructable from current RoomShard owners.

A RoomCoordinator must not install a stale forwarding plan over the current
connection owner's plan. Commands need explicit author/origin, target, and
incarnation checks; exact wire representation and conflict-resolution rules
are design work, **not implemented guarantees**. In particular, `ConnectionId`
ordering is a local replacement discriminator, not an across-node exclusive
lease. During a membership split, competing coordinators and duplicate
participant identities are possible unless an explicit admission/fencing
protocol prevents them. Eventual convergence alone cannot promise globally
unique participants *at every instant*.

## Failure and reconstruction

- **Client disconnect or connection replacement:** the connection owner
  retires that incarnation's publications and routes. The client reconnects
  and resubmits its complete desired state; stale prior-incarnation events
  must not resurrect old publications.
- **Owning node failure:** affected clients establish new WebRTC connections
  and reapply their desired state. No server promises to move live ICE, DTLS,
  SRTP, RTP or congestion-control state across nodes.
- **Coordinator failure or membership change:** current RoomShard owners
  provide complete snapshots to the newly selected coordinator; plans
  converge from current live facts instead of replaying a durable event log.
- **Delayed traffic / node restarts:** destination route slot epochs protect
  local slot reuse. Cross-node forwarding also needs restart-safe node
  incarnation checks so a reused node ID or route slot cannot accept stale
  packets or control decisions.
- **Network split:** local owners may continue maintaining their own live
  connections. The room-wide view may be incomplete; global uniqueness and
  admission behavior under split membership need an explicit contract before
  declaring cluster support production ready.

Recovery is **reconnection and reconstruction**, not transparent media
resumption. A catalog or mapping delivered to a client is an observed
snapshot/assignment, not the client's source of desired state. See the
[simulation signaling contract](../../pulsebeam-simulator/docs/sim.md#signaling-contract-under-test).

## Required clustering proof before production

Adversarial simulation must cover membership disagreement, competing
coordinators, stale room-fragment snapshots, revision reordering,
node-incarnation reuse, duplicate participant admission, cross-node track
publication/withdrawal, lost plan updates, failed node recovery and repeated
client reconnection. Assert actual room and per-shard projection convergence,
publication uniqueness after reconciliation, correct local fanout and
absence of delivery to retired owners—not merely that a packet was received.

For existing implemented packet behavior, use [routing.md](routing.md).
For canonical identity/address naming, use [naming.md](naming.md).
For execution ownership and scheduler behavior, use
[shard-owned-dataplane.md](shard-owned-dataplane.md).
