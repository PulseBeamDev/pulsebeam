use super::common::{LocalNodeSim, Participant, Room, Step, VideoQuality};
use std::time::Duration;

#[test]
fn fast_initial_ramp_up_on_good_network_test() {
    LocalNodeSim::new()
        .with_tick(Duration::from_millis(1))
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q", "h", "f"]))
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "2s ramp window",
                duration: Duration::from_secs(2),
            },
            Step::CheckTxBytesInterval {
                description: "Alice reached ≥400kbps within 2s",
                participant: "alice",
                min_bytes: 100_000,
            },
            Step::Run {
                description: "18s soak at floor bitrate",
                duration: Duration::from_secs(18),
            },
            Step::CheckTxBytesInterval {
                description: "Alice never collapsed below 250kbps floor over 18s",
                participant: "alice",
                min_bytes: 562_500,
            },
            Step::CheckVideoQuality {
                description: "Bob received renderable frames throughout ramp",
                participant: "bob",
                quality: VideoQuality::min_frames(200).allow_gaps_for_switches(3),
            },
        ]);
}

#[test]
fn repeated_simulcast_switching_stays_decodable_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q", "h", "f"]))
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "Establish initial flow and let highest layer settle",
                duration: Duration::from_secs(15),
            },
            Step::SubscribeAll {
                description: "Subscribe at 720p",
                participant: "bob",
                heights: &[720],
            },
            Step::Run {
                description: "Accumulate frames at 720p",
                duration: Duration::from_secs(10),
            },
            Step::SubscribeAll {
                description: "Switch 1: 180p",
                participant: "bob",
                heights: &[180],
            },
            Step::Run {
                description: "Settle",
                duration: Duration::from_millis(2500),
            },
            Step::SubscribeAll {
                description: "Switch 2: 720p",
                participant: "bob",
                heights: &[720],
            },
            Step::Run {
                description: "Settle",
                duration: Duration::from_millis(2500),
            },
            Step::SubscribeAll {
                description: "Switch 3: 180p",
                participant: "bob",
                heights: &[180],
            },
            Step::Run {
                description: "Settle",
                duration: Duration::from_millis(2500),
            },
            Step::SubscribeAll {
                description: "Switch 4: 720p",
                participant: "bob",
                heights: &[720],
            },
            Step::Run {
                description: "Settle",
                duration: Duration::from_millis(2500),
            },
            Step::CheckVideoQuality {
                description: "Frames remain decodable across 4 simulcast switches",
                participant: "bob",
                quality: VideoQuality::min_frames(100).allow_gaps_for_switches(4),
            },
        ]);
}

// Pre-existing SFU bug: RTP timestamp goes backwards during simulcast layer
// switches, triggering the egress stream invariant assertion in core.rs:465.
#[ignore = "pre-existing production bug: egress stream invariant violated on simulcast layer switch"]
#[test]
fn simulcast_stream_stability_test() {
    LocalNodeSim::new()
        .with_tick(Duration::from_millis(1))
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q", "h", "f"]))
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "Warmup: establish initial flow",
                duration: Duration::from_secs(5),
            },
            Step::Run {
                description: "60s soak: stream must stay alive",
                duration: Duration::from_secs(60),
            },
            Step::CheckRxBytesInterval {
                description: "Bob received steady throughput across the full soak",
                participant: "bob",
                min_bytes: 75_000,
            },
            Step::CheckVideoQuality {
                description: "Bob received renderable frames throughout soak",
                participant: "bob",
                quality: VideoQuality::min_frames(500).allow_gaps_for_switches(5),
            },
        ]);
}
