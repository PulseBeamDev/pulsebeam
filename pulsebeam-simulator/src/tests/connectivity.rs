use super::common::{LinkProfile, LocalNodeSim, Participant, Room, Step, VideoQuality};
use std::time::Duration;

#[test]
fn simulation_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::single_publisher("alice"))
                .with_participant(Participant::subscriber("bob"))
                .with_participant(Participant::subscriber("carol"))
                .with_participant(Participant::single_publisher("churn1").starts_disconnected())
                .with_participant(Participant::single_publisher("churn2").starts_disconnected()),
        )
        .run(vec![
            Step::Run {
                description: "Establish initial flow",
                duration: Duration::from_secs(20),
            },
            Step::CheckRxBytes {
                description: "Bob receives video",
                participant: "bob",
                min_bytes: 1,
            },
            Step::Partition {
                description: "Alice ↔ server",
                from: "alice",
                to: "server",
            },
            Step::Hold {
                description: "Hold Bob packets",
                from: "bob",
                to: "server",
            },
            Step::Run {
                description: "Partitioned + held",
                duration: Duration::from_secs(10),
            },
            Step::Release {
                description: "Release Bob",
                from: "bob",
                to: "server",
            },
            Step::Repair {
                description: "Restore Alice",
                from: "alice",
                to: "server",
            },
            Step::Run {
                description: "Recovery",
                duration: Duration::from_secs(5),
            },
            Step::Join {
                description: "Churn 1 joins",
                participant: "churn1",
            },
            Step::Join {
                description: "Churn 2 joins",
                participant: "churn2",
            },
            Step::Run {
                description: "Churn cycle",
                duration: Duration::from_secs(6),
            },
            Step::Disconnect {
                description: "Churn 1 leaves",
                participant: "churn1",
            },
            Step::Disconnect {
                description: "Churn 2 leaves",
                participant: "churn2",
            },
            Step::Disconnect {
                description: "Alice disconnects",
                participant: "alice",
            },
            Step::Disconnect {
                description: "Bob disconnects",
                participant: "bob",
            },
            Step::Disconnect {
                description: "Carol disconnects",
                participant: "carol",
            },
            Step::Run {
                description: "Wait for disconnections",
                duration: Duration::from_secs(20),
            },
            Step::CheckNotConnected {
                description: "Alice is disconnected",
                participant: "alice",
            },
        ]);
}

#[test]
fn tcp_simulation_test() {
    LocalNodeSim::new()
        .with_tcp_only()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::single_publisher("alice"))
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "Establish TCP flow",
                duration: Duration::from_secs(40),
            },
            Step::CheckRxBytes {
                description: "Bob receives over TCP",
                participant: "bob",
                min_bytes: 1,
            },
            Step::Disconnect {
                description: "Alice disconnects",
                participant: "alice",
            },
            Step::Disconnect {
                description: "Bob disconnects",
                participant: "bob",
            },
            Step::Run {
                description: "Wait for cleanup",
                duration: Duration::from_secs(20),
            },
            Step::CheckNotConnected {
                description: "Alice is disconnected",
                participant: "alice",
            },
        ]);
}

fn cross_shard_media_room() -> Room {
    Room::new("cross-shard-media")
        .with_participant(Participant::single_publisher("publisher"))
        .with_participant(Participant::subscriber("subscriber"))
}

#[test]
/// Replays a failing run with `PULSEBEAM_SIM_SEED=<seed>` from the test output.
fn controller_stall_keeps_established_media_alive() {
    LocalNodeSim::new()
        .with_shards(2)
        .with_room(cross_shard_media_room())
        .run(vec![
            Step::Run {
                description: "establish media before the controller stall",
                duration: Duration::from_secs(5),
            },
            Step::StallController {
                duration: Duration::from_secs(5),
            },
            Step::CheckRxBytesInterval {
                description: "media continues while control is stalled",
                participant: "subscriber",
                min_bytes: 1,
            },
        ]);
}

/// A datagram delivered to a shard that does not own its route reaches the
/// owner instead of being dropped.
///
/// Replays a failing run with `PULSEBEAM_SIM_SEED=<seed>` from the test output.
#[test]
fn wrong_owner_forwards_and_media_continues() {
    LocalNodeSim::new()
        .with_shards(2)
        .with_room(cross_shard_media_room())
        .run(vec![
            Step::Run {
                description: "establish the media path",
                duration: Duration::from_secs(5),
            },
            Step::CheckRoutingCounterSettles {
                description: "steering has pinned the flows, so forwarding has stopped",
                name: "shard_wrong_owner_forward",
                over: Duration::from_secs(2),
            },
            Step::SendToWrongShard {
                description: "inject one datagram into a foreign shard",
                participant: "publisher",
            },
            Step::CheckRoutingCounterAtLeast {
                description: "the foreign datagram is forwarded to its owner",
                name: "shard_wrong_owner_forward",
                min: 1,
            },
            Step::CheckRxBytes {
                description: "the participant's own media remains unaffected",
                participant: "subscriber",
                min_bytes: 1,
            },
        ]);
}

/// Cross-shard forwarding is a bootstrap cost, not a steady-state one.
///
/// Steering is a cache: a miss lands on whatever the kernel's tuple hash picked
/// and userspace forwards it to the route's owner. Once the flow authenticates,
/// control pins it and the forwarding stops. Nothing else notices if that
/// pinning never happens — media still arrives, just across a core boundary on
/// every packet — so the property worth holding is that the rate reaches zero.
#[test]
fn steering_stops_cross_shard_forwarding_once_flows_authenticate() {
    LocalNodeSim::new()
        .with_shards(4)
        .with_room(cross_shard_media_room())
        .run(vec![
            Step::Run {
                description: "let every flow authenticate",
                duration: Duration::from_secs(8),
            },
            Step::CheckRoutingCounterSettles {
                description: "no packet crosses a shard boundary at steady state",
                name: "shard_wrong_owner_forward",
                over: Duration::from_secs(3),
            },
            Step::CheckRxBytesInterval {
                description: "and media is still flowing while it does not",
                participant: "subscriber",
                min_bytes: 1,
            },
        ]);
}

#[test]
/// Replays a failing run with `PULSEBEAM_SIM_SEED=<seed>` from the test output.
fn failed_materialization_does_not_connect_the_participant() {
    LocalNodeSim::new()
        .with_shards(2)
        .with_room(
            Room::new("materialization-failure")
                .with_participant(Participant::single_publisher("publisher"))
                .with_participant(Participant::subscriber("subscriber").starts_disconnected()),
        )
        .run(vec![
            Step::Run {
                description: "establish the publisher before the injected failure",
                duration: Duration::from_secs(5),
            },
            Step::FailNextMaterialization {
                description: "fail the next participant materialization",
            },
            Step::Join {
                description: "attempt the failed materialization",
                participant: "subscriber",
            },
            Step::Run {
                description: "allow the failed command to drain",
                duration: Duration::from_secs(2),
            },
            Step::CheckRoutingCounter {
                description: "the injected failure was consumed",
                name: "materialization_failed",
                exact: 1,
            },
            Step::CheckRoutingCounter {
                description: "failed materialization leaves no participant key behind",
                name: "materialization_orphan",
                exact: 0,
            },
        ]);
}

/// Replays a failing run with `PULSEBEAM_SIM_SEED=<seed>` from the test output.
#[test]
fn track_observation_is_not_forwarded_before_plan() {
    LocalNodeSim::new()
        .with_shards(2)
        .with_room(cross_shard_media_room())
        .run(vec![
            Step::Run {
                description: "settle observation, publication, and plan in order",
                duration: Duration::from_secs(8),
            },
            Step::CheckMediaRouted {
                description: "steady-state forwarding uses a compiled plan",
                participant: "subscriber",
            },
        ]);

    for (lane, stage, origin) in [
        ("video", "plan", "local"),
        ("video", "plan", "remote"),
        ("audio", "plan", "local"),
        ("audio", "plan", "remote"),
        ("data", "plan", "local"),
        ("data", "plan", "remote"),
        ("reliable", "plan", "local"),
        ("reliable", "plan", "remote"),
    ] {
        assert_eq!(
            pulsebeam::sim_metrics::routing_drop(lane, stage, origin),
            0,
            "steady-state routing must not observe {lane} before the {stage}"
        );
    }
}

/// Reproduces the Chrome-with-UDP-disabled failure: with two shards the hash of
/// a client's `peer_addr` and the hash of `room_id` can land on different shards,
/// causing TCP egress to be silently dropped.
///
/// The fix routes egress cross-shard via `CrossShardEvent::TcpEgressForward`.
#[test]
fn tcp_multi_shard_simulation_test() {
    LocalNodeSim::new()
        .with_shards(2)
        .with_tcp_only()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::single_publisher("alice"))
                .with_participant(Participant::subscriber("bob"))
                .with_participant(Participant::subscriber("carol"))
                .with_participant(Participant::subscriber("dave")),
        )
        .run(vec![
            Step::Run {
                description: "Establish multi-shard TCP flow",
                duration: Duration::from_secs(50),
            },
            Step::CheckRxBytes {
                description: "Bob receives over multi-shard TCP",
                participant: "bob",
                min_bytes: 1,
            },
            Step::Disconnect {
                description: "Alice disconnects",
                participant: "alice",
            },
            Step::Disconnect {
                description: "Bob disconnects",
                participant: "bob",
            },
            Step::Disconnect {
                description: "Carol disconnects",
                participant: "carol",
            },
            Step::Disconnect {
                description: "Dave disconnects",
                participant: "dave",
            },
            Step::Run {
                description: "Wait for cleanup",
                duration: Duration::from_secs(20),
            },
            Step::CheckNotConnected {
                description: "Alice is disconnected",
                participant: "alice",
            },
        ]);
}

#[test]
fn network_impairment_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::single_publisher("alice"))
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "Establish baseline flow",
                duration: Duration::from_secs(30),
            },
            Step::CheckVideoQuality {
                description: "Bob receives renderable video before partition",
                participant: "bob",
                quality: VideoQuality::min_frames(100),
            },
            Step::Partition {
                description: "Alice ↔ server outage begins",
                from: "alice",
                to: "server",
            },
            Step::Run {
                description: "Partitioned period",
                duration: Duration::from_secs(10),
            },
            Step::Repair {
                description: "Restore Alice ↔ server",
                from: "alice",
                to: "server",
            },
            Step::Run {
                description: "Recovery period",
                duration: Duration::from_secs(30),
            },
            Step::CheckVideoQuality {
                description: "Bob receives renderable video after recovery",
                participant: "bob",
                quality: VideoQuality::min_frames(200).allow_gaps(1),
            },
        ]);
}

#[test]
fn reconnection_recovery_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::single_publisher("alice"))
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "Establish initial flow",
                duration: Duration::from_secs(15),
            },
            Step::CheckVideoQuality {
                description: "Bob receives renderable video before partition",
                participant: "bob",
                quality: VideoQuality::min_frames(50),
            },
            Step::Partition {
                description: "Alice ↔ server network failure",
                from: "alice",
                to: "server",
            },
            Step::Run {
                description: "Partitioned period",
                duration: Duration::from_secs(10),
            },
            Step::Repair {
                description: "Lift partition — agent should auto-reconnect",
                from: "alice",
                to: "server",
            },
            Step::Run {
                description: "Recovery period",
                duration: Duration::from_secs(20),
            },
            Step::CheckVideoQuality {
                description: "Bob receives renderable video after Alice reconnects",
                participant: "bob",
                quality: VideoQuality::min_frames(100).allow_gaps(1),
            },
        ]);
}

#[test]
fn churn_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::single_publisher("stable"))
                .with_participant(Participant::subscriber("churner")),
        )
        .run(vec![
            Step::Run {
                description: "Initial session",
                duration: Duration::from_secs(10),
            },
            Step::CheckVideoQuality {
                description: "Churner receives renderable video in first session",
                participant: "churner",
                quality: VideoQuality::min_frames(50),
            },
            Step::Disconnect {
                description: "Churner leaves",
                participant: "churner",
            },
            Step::Run {
                description: "Pause between sessions",
                duration: Duration::from_secs(5),
            },
            Step::Reconnect {
                description: "Churner rejoins",
                participant: "churner",
            },
            Step::Run {
                description: "Second session",
                duration: Duration::from_secs(10),
            },
            Step::CheckVideoQuality {
                description: "Churner receives renderable video in second session",
                participant: "churner",
                quality: VideoQuality::min_frames(100),
            },
        ]);
}

#[test]
fn abrupt_exit_chaos_test() {
    let room = Room::new("room1")
        .with_participant(Participant::single_publisher("stable"))
        .with_participant(Participant::subscriber("observer"))
        .with_participant(Participant::single_publisher("crasher1").starts_disconnected())
        .with_participant(Participant::single_publisher("crasher2").starts_disconnected())
        .with_participant(Participant::single_publisher("crasher3").starts_disconnected())
        .with_participant(Participant::single_publisher("crasher4").starts_disconnected());

    LocalNodeSim::new().with_room(room).run(vec![
        Step::Run {
            description: "Stable pair establishes",
            duration: Duration::from_secs(3),
        },
        Step::Join {
            description: "Crasher 1 enters",
            participant: "crasher1",
        },
        Step::Run {
            description: "Crasher 1 active",
            duration: Duration::from_secs(2),
        },
        Step::AbruptExit {
            description: "Crasher 1 exits",
            participant: "crasher1",
        },
        Step::Run {
            description: "Gap",
            duration: Duration::from_secs(2),
        },
        Step::Join {
            description: "Crasher 2 enters",
            participant: "crasher2",
        },
        Step::Run {
            description: "Crasher 2 active",
            duration: Duration::from_secs(2),
        },
        Step::AbruptExit {
            description: "Crasher 2 exits",
            participant: "crasher2",
        },
        Step::Run {
            description: "Gap",
            duration: Duration::from_secs(2),
        },
        Step::Join {
            description: "Crasher 3 enters",
            participant: "crasher3",
        },
        Step::Run {
            description: "Crasher 3 active",
            duration: Duration::from_secs(2),
        },
        Step::AbruptExit {
            description: "Crasher 3 exits",
            participant: "crasher3",
        },
        Step::Run {
            description: "Gap",
            duration: Duration::from_secs(2),
        },
        Step::Join {
            description: "Crasher 4 enters",
            participant: "crasher4",
        },
        Step::Run {
            description: "Crasher 4 active",
            duration: Duration::from_secs(2),
        },
        Step::AbruptExit {
            description: "Crasher 4 exits",
            participant: "crasher4",
        },
        Step::Run {
            description: "Final observation window",
            duration: Duration::from_secs(8),
        },
        Step::CheckVideoQuality {
            description: "Observer kept receiving renderable frames despite chaos",
            participant: "observer",
            quality: VideoQuality::min_frames(100)
                .allow_gaps(4)
                .allow_missing_parameter_sets(1),
        },
    ]);
}

/// Media crossing a shard boundary, end to end, over UDP.
///
/// With `room_shard_slot(1)` the publisher and each subscriber land on
/// *different* shards, so every forwarded packet is addressed by a route the
/// destination allocated, wrapped in a `MediaEnvelope`, resolved by index and
/// epoch, and restamped onto the receiving shard's timeline. A room below the
/// spill threshold is co-located and reaches none of that — which is why the
/// older multi-shard test, with four participants and a slot of sixteen, never
/// exercised it despite the name.
///
/// Over UDP, so the shard each participant lands on is chosen by the
/// `SO_REUSEPORT` group hashing its 4-tuple — the same mechanism a deployment
/// relies on, rather than the TCP fallback the multi-shard tests started on.
#[test]
fn cross_shard_video_is_forwarded_decodably_test() {
    LocalNodeSim::new()
        .with_shards(2)
        .with_room(
            Room::new("room1")
                .with_participant(Participant::single_publisher("alice"))
                .with_participant(Participant::subscriber("bob"))
                .with_participant(Participant::subscriber("carol")),
        )
        .run(vec![
            Step::Run {
                description: "Alice publishes; Bob and Carol subscribe from other shards",
                duration: Duration::from_secs(20),
            },
            Step::CheckCrossShardMedia {
                description: "media genuinely crossed a shard boundary",
                min_frames: 100,
            },
            Step::CheckVideoQuality {
                description: "Bob decodes a stream that crossed a shard boundary",
                participant: "bob",
                quality: VideoQuality::min_frames(100).allow_gaps(5),
            },
            Step::CheckVideoQuality {
                description: "Carol decodes it too, over her own route",
                participant: "carol",
                quality: VideoQuality::min_frames(100).allow_gaps(5),
            },
        ]);
}

/// The SFU keeps serving video while route installs are failing under it.
///
/// Every fallible internal call has a rollback written beside it, and none of them had ever run:
/// the route table only fills at a participant count no plan reaches, so the four callers'
/// recovery paths were dead code that happened to compile. `with_buggify` makes the failure
/// happen on purpose.
///
/// The claim is deliberately about recovery rather than perfection. Some subscriptions will not
/// install while the table is refusing, and that is the correct response to exhaustion - what may
/// not happen is the node wedging, losing a stable stream, or tripping an assertion on the way
/// through.
///
/// Single shard for now. Adding `.with_shards(3)` reaches the cross-shard installers and trips
/// `core.rs`'s "no reverse route for a remotely published track" immediately: a failed reverse
/// install publishes the track with no reverse handle, so keyframe requests for it are dropped for
/// its whole life. That is a real defect with an open design question - whether a track that
/// cannot be addressed should be announced at all - and it is not this plan's to answer.
#[test]
fn video_survives_failing_route_installs_test() {
    LocalNodeSim::new()
        .with_buggify(300)
        .with_room(
            Room::new("room1")
                .with_participant(Participant::single_publisher("stable"))
                .with_participant(Participant::subscriber("observer"))
                .with_participant(Participant::single_publisher("joiner").starts_disconnected()),
        )
        .run(vec![
            Step::Run {
                description: "Establish the stable pair",
                duration: Duration::from_secs(10),
            },
            Step::Join {
                description: "Another publisher arrives while installs are failing",
                participant: "joiner",
            },
            Step::Run {
                description: "Churn through the failures",
                duration: Duration::from_secs(10),
            },
            Step::AbruptExit {
                description: "And leaves without signalling",
                participant: "joiner",
            },
            Step::Run {
                description: "Recover",
                duration: Duration::from_secs(15),
            },
            Step::CheckVideoQuality {
                description: "The observer still receives renderable video throughout",
                participant: "observer",
                quality: VideoQuality::min_frames(100),
            },
        ]);

    // Without this the plan passes hardest when it injects nothing. At 80 per thousand and a
    // handful of install calls it injected nothing at all on the first seed tried, and looked
    // exactly like a pass.
    let (_, fired) = pulsebeam_runtime::buggify::coverage();
    assert!(
        !fired.is_empty(),
        "no failure was injected, so this plan asserted only that the happy path works"
    );
}

/// Every declared failure point is reachable, and the injector actually injects.
///
/// A `buggify!` site that no plan reaches is a failure path still untested, and it looks exactly
/// like one that is covered - silence either way. This turns the declared sites into a list that
/// has to be kept honest: reaching zero of them, or firing none of them, means the mechanism has
/// quietly stopped doing anything.
#[test]
fn every_declared_failure_point_is_reachable_test() {
    LocalNodeSim::new()
        .with_buggify(500)
        .with_room(
            Room::new("room1")
                .with_participant(Participant::single_publisher("alice"))
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![Step::Run {
            description: "Enough traffic to reach the route table",
            duration: Duration::from_secs(10),
        }]);

    let (seen, fired) = pulsebeam_runtime::buggify::coverage();
    assert!(
        !seen.is_empty(),
        "no buggify site was reached, so failure injection is testing nothing"
    );
    assert!(
        !fired.is_empty(),
        "buggify sites were reached ({seen:?}) but none fired at 50%, so injection is inert"
    );
}

/// A publisher who leaves and comes back is shown to a viewer who never went away.
///
/// The reconnect churn plans all move the *subscriber*. This moves the publisher, which is a
/// different path: the viewer keeps its slot and its subscription, and the room hands that slot a
/// new track id belonging to a participant it has never seen. Nothing was covering it.
#[test]
fn a_rejoining_publisher_is_shown_to_an_existing_viewer_test() {
    LocalNodeSim::new()
        .with_link(LinkProfile::cellular())
        .with_shards(4)
        .with_room(
            Room::new("room1")
                .with_participant(Participant::single_publisher("alice"))
                .with_participant(Participant::subscriber("viewer")),
        )
        .run(vec![
            Step::Run {
                description: "Alice is on screen",
                duration: Duration::from_secs(8),
            },
            Step::CheckVideoQuality {
                description: "The viewer can see her",
                participant: "viewer",
                quality: VideoQuality::min_frames(50),
            },
            Step::Disconnect {
                description: "Alice drops out",
                participant: "alice",
            },
            Step::Run {
                description: "The tile is empty",
                duration: Duration::from_secs(3),
            },
            Step::Reconnect {
                description: "Alice comes back, as a participant the room has never seen",
                participant: "alice",
            },
            Step::Run {
                description: "Settle on the new publisher",
                duration: Duration::from_secs(10),
            },
            Step::CheckVideoQualityInterval {
                description: "The viewer can see the publisher who replaced her",
                participant: "viewer",
                quality: VideoQuality::min_frames(50),
            },
            Step::CheckMediaRouted {
                description: "And nothing was thrown away on the way in",
                participant: "viewer",
            },
        ]);
}

/// A connection that drops and recovers is the same participant throughout.
///
/// The path a real client takes after a network blip, and nothing covered it: every other churn
/// plan tears the client down and joins again, which mints a *new* participant id and is a
/// different thing entirely. A reconnect keeps the id and changes only the connection generation -
/// the server does this over `PATCH` with `If-Match: <etag>`, and rejects an update that does not
/// name the generation it replaces.
///
/// **Ignored: reconnect is designed but not implemented end to end.** Identity is stable - that
/// part passes - but the viewer never sees Alice again, and the reason is upstream of the client:
///
/// - the SFU destroys the participant as soon as ICE drops (`Participant core disconnecting ...
///   reason=ICE connection disconnected`), so by the time the network returns there is nothing
///   left to `PATCH` and the generation model has nothing to attach to;
/// - and the agent makes no reconnect attempt at all - zero `Sending SDP Offer (Update)` in a run.
///
/// The agent's missing `If-Match` header is fixed and was a real defect on this path, but it is
/// only the last step of three. Un-ignore once the SFU holds a disconnected participant open long
/// enough to be reclaimed, and the agent actually tries.
#[ignore = "reconnect is not implemented end to end: the SFU drops the participant on ICE disconnect"]
#[test]
fn a_dropped_connection_recovers_as_the_same_participant_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::single_publisher("alice"))
                .with_participant(Participant::subscriber("viewer")),
        )
        .run(vec![
            Step::Run {
                description: "Alice is on screen",
                duration: Duration::from_secs(6),
            },
            Step::Partition {
                description: "Alice's network drops",
                from: "alice",
                to: "server",
            },
            Step::Run {
                description: "Long enough for the connection to be given up on",
                duration: Duration::from_secs(12),
            },
            Step::Repair {
                description: "Her network comes back",
                from: "alice",
                to: "server",
            },
            Step::Run {
                description: "She reconnects",
                duration: Duration::from_secs(12),
            },
            Step::CheckIdentityStable {
                description: "Alice reconnected rather than rejoining as somebody new",
                participant: "alice",
            },
            Step::CheckVideoQualityInterval {
                description: "And the viewer can see her again",
                participant: "viewer",
                quality: VideoQuality::min_frames(50),
            },
        ]);
}
