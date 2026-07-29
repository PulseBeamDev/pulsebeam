use super::common::{LocalNodeSim, Participant, Room, Step, VideoQuality};
use std::time::Duration;

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
        .with_participant(Participant::single_publisher("crasher4").starts_disconnected())
        .with_participant(Participant::single_publisher("crasher5").starts_disconnected())
        .with_participant(Participant::single_publisher("crasher6").starts_disconnected())
        .with_participant(Participant::single_publisher("crasher7").starts_disconnected())
        .with_participant(Participant::single_publisher("crasher8").starts_disconnected());

    LocalNodeSim::new()
        .with_rng_seed(0xC0FFEE)
        .with_room(room)
        .run(vec![
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
                description: "Crasher 1 exits without signaling",
                participant: "crasher1",
            },
            Step::Run {
                description: "Gap before crasher 2",
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
                description: "Crasher 2 exits without signaling",
                participant: "crasher2",
            },
            Step::Run {
                description: "Gap before crasher 3",
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
                description: "Crasher 3 exits without signaling",
                participant: "crasher3",
            },
            Step::Run {
                description: "Gap before crasher 4",
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
                description: "Crasher 4 exits without signaling",
                participant: "crasher4",
            },
            Step::Run {
                description: "Gap before crasher 5",
                duration: Duration::from_secs(2),
            },
            Step::Join {
                description: "Crasher 5 enters",
                participant: "crasher5",
            },
            Step::Run {
                description: "Crasher 5 active",
                duration: Duration::from_secs(2),
            },
            Step::AbruptExit {
                description: "Crasher 5 exits without signaling",
                participant: "crasher5",
            },
            Step::Run {
                description: "Gap before crasher 6",
                duration: Duration::from_secs(2),
            },
            Step::Join {
                description: "Crasher 6 enters",
                participant: "crasher6",
            },
            Step::Run {
                description: "Crasher 6 active",
                duration: Duration::from_secs(2),
            },
            Step::AbruptExit {
                description: "Crasher 6 exits without signaling",
                participant: "crasher6",
            },
            Step::Run {
                description: "Gap before crasher 7",
                duration: Duration::from_secs(2),
            },
            Step::Join {
                description: "Crasher 7 enters",
                participant: "crasher7",
            },
            Step::Run {
                description: "Crasher 7 active",
                duration: Duration::from_secs(2),
            },
            Step::AbruptExit {
                description: "Crasher 7 exits without signaling",
                participant: "crasher7",
            },
            Step::Run {
                description: "Gap before crasher 8",
                duration: Duration::from_secs(2),
            },
            Step::Join {
                description: "Crasher 8 enters",
                participant: "crasher8",
            },
            Step::Run {
                description: "Crasher 8 active",
                duration: Duration::from_secs(2),
            },
            Step::AbruptExit {
                description: "Crasher 8 exits without signaling",
                participant: "crasher8",
            },
            Step::Run {
                description: "Final observation window",
                duration: Duration::from_secs(8),
            },
            Step::CheckVideoQuality {
                description: "Observer kept receiving renderable frames despite chaos",
                participant: "observer",
                quality: VideoQuality::min_frames(100).allow_gaps_for_switches(8),
            },
        ]);
}
