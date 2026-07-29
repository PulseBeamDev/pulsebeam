use super::common::{LocalNodeSim, Participant, Room, Step};
use std::time::Duration;

#[test]
fn simulation_test() {
    LocalNodeSim::new()
        .with_tick(Duration::from_micros(100))
        .with_room(
            Room::new("room1")
                .with_participant(Participant::single_publisher("alice"))
                .with_participant(Participant::subscriber("bob"))
                .with_participant(Participant::subscriber("carol"))
                .with_participant(Participant::single_publisher("churn1").starts_disconnected())
                .with_participant(Participant::single_publisher("churn2").starts_disconnected())
                .with_participant(Participant::single_publisher("churn3").starts_disconnected())
                .with_participant(Participant::single_publisher("churn4").starts_disconnected()),
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
                description: "Churn 1a joins",
                participant: "churn1",
            },
            Step::Join {
                description: "Churn 1b joins",
                participant: "churn2",
            },
            Step::Run {
                description: "Churn cycle 1",
                duration: Duration::from_secs(6),
            },
            Step::Disconnect {
                description: "Churn 1a leaves",
                participant: "churn1",
            },
            Step::Disconnect {
                description: "Churn 1b leaves",
                participant: "churn2",
            },
            Step::Run {
                description: "Between churn cycles",
                duration: Duration::from_secs(4),
            },
            Step::Join {
                description: "Churn 2a joins",
                participant: "churn3",
            },
            Step::Join {
                description: "Churn 2b joins",
                participant: "churn4",
            },
            Step::Run {
                description: "Churn cycle 2",
                duration: Duration::from_secs(6),
            },
            Step::Disconnect {
                description: "Churn 2a leaves",
                participant: "churn3",
            },
            Step::Disconnect {
                description: "Churn 2b leaves",
                participant: "churn4",
            },
            Step::Run {
                description: "Settle after churn",
                duration: Duration::from_secs(5),
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
            Step::CheckNotConnected {
                description: "Bob is disconnected",
                participant: "bob",
            },
            Step::CheckNotConnected {
                description: "Carol is disconnected",
                participant: "carol",
            },
        ]);
}

#[test]
fn tcp_simulation_test() {
    LocalNodeSim::new()
        .with_tick(Duration::from_micros(100))
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
            Step::CheckNotConnected {
                description: "Bob is disconnected",
                participant: "bob",
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
        .with_tick(Duration::from_micros(100))
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
