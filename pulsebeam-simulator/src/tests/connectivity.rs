use super::common::{LocalNodeSim, Participant, Room, Step, VideoQuality};
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

/// Reproduces the Chrome-with-UDP-disabled failure: with two shards the hash of
/// a client's `peer_addr` and the hash of `room_id` can land on different shards,
/// causing TCP egress to be silently dropped.
///
/// The fix routes egress cross-shard via `CrossShardEvent::TcpEgressForward`.
#[test]
fn tcp_multi_shard_simulation_test() {
    LocalNodeSim::new()
        .with_shards(2)
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

/// Media flows for a participant that joined over the authenticated JSON API.
///
/// Proves the JSON join is a complete substitute for the SDP one: real HTTP, real ICE, real DTLS,
/// real RTP, differing only in how the offer and answer were carried.
#[test]
fn authenticated_join_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(
                    Participant::single_publisher("alice").authenticated_as("user_alice"),
                )
                .with_participant(Participant::subscriber("bob").authenticated_as("user_bob")),
        )
        .run(vec![
            Step::Run {
                description: "Authenticated publisher and subscriber establish flow",
                duration: Duration::from_secs(15),
            },
            Step::CheckVideoQuality {
                description: "Bob renders Alice's video after a JSON join",
                participant: "bob",
                quality: VideoQuality::min_frames(50),
            },
        ]);
}

/// A participant that survives a cold restart of the SFU keeps its identity.
///
/// The node is torn down and started again with no memory of anyone. The client is not told; it
/// discovers the loss through ICE and resumes on its own. What must hold afterwards is that it is
/// the *same* participant, because every TrackId is derived from the participant id -- so
/// subscribers see the tracks come back rather than a stranger's arrive.
///
/// The assertions are deliberately interval-based. A cumulative frame count would be satisfied by
/// frames received before the restart and would pass even if nothing ever recovered, which is
/// exactly how the first version of this test fooled its author.
#[test]
fn resume_across_node_restart_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(
                    Participant::single_publisher("alice").authenticated_as("user_alice"),
                )
                .with_participant(Participant::subscriber("bob").authenticated_as("user_bob")),
        )
        .run(vec![
            Step::Run {
                description: "Establish flow before the restart",
                duration: Duration::from_secs(15),
            },
            Step::CheckVideoQuality {
                description: "Bob renders video before the restart",
                participant: "bob",
                quality: VideoQuality::min_frames(50),
            },
            Step::RestartNode {
                description: "Cold-restart the SFU; nobody is told",
            },
            // The interval form is essential here: a cumulative frame count would be satisfied by
            // frames received before the restart and would pass even if nothing ever recovered.
            Step::Run {
                description: "Clients notice through ICE and resume unaided",
                duration: Duration::from_secs(180),
            },
            Step::CheckTxBytesInterval {
                description: "The publisher sends again on its rebuilt connection",
                participant: "alice",
                min_bytes: 20_000,
            },
            Step::CheckRxBytesInterval {
                description: "The subscriber receives again in the window after the rebuild",
                participant: "bob",
                min_bytes: 20_000,
            },
            Step::CheckVideoQuality {
                description: "And the video it receives is renderable, not just bytes",
                participant: "bob",
                // One gap is expected: the restart breaks the egress sequence exactly once.
                quality: VideoQuality::min_frames(80).allow_gaps(1),
            },
        ]);
}
