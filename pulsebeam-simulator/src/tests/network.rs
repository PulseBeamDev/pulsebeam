use super::common::{LocalNodeSim, Participant, Room, Step, VideoQuality};
use std::time::Duration;

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
                quality: VideoQuality::min_frames(200),
            },
        ]);
}
