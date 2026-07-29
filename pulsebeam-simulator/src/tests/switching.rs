use super::common::{LocalNodeSim, Participant, Room, Step, VideoQuality};
use std::time::Duration;

/// Forces the SFU to switch a subscriber between simulcast layers 12 times and
/// asserts the subscriber can still decode what comes out.
///
/// The decode-side verdict comes from str0m's own depacketizer on the receiving
/// client: `contiguous` is false whenever a frame followed a sequence-number
/// hole, and `is_keyframe` marks a decodable entry point. A switch that drops
/// parameter sets, reuses a timestamp, or renumbers around missing packets shows
/// up here and nowhere in a bytes-received counter.
#[test]
fn repeated_simulcast_switching_stays_decodable_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q", "h", "f"]))
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            // Establish the stream and let the highest layer settle.
            Step::Run {
                description: "Establish initial flow",
                duration: Duration::from_secs(15),
            },
            Step::SubscribeAll {
                description: "Subscribe to publisher track at 720p",
                participant: "bob",
                heights: &[720],
            },
            Step::Run {
                description: "Let 720p subscription settle and accumulate frames",
                duration: Duration::from_secs(20),
            },
            // 12 strictly alternating switches (each forces a full SSRC change).
            Step::SubscribeAll {
                description: "Switch 1: 180p",
                participant: "bob",
                heights: &[180],
            },
            Step::Run {
                description: "Switch 1 settle",
                duration: Duration::from_millis(2500),
            },
            Step::SubscribeAll {
                description: "Switch 2: 720p",
                participant: "bob",
                heights: &[720],
            },
            Step::Run {
                description: "Switch 2 settle",
                duration: Duration::from_millis(2500),
            },
            Step::SubscribeAll {
                description: "Switch 3: 180p",
                participant: "bob",
                heights: &[180],
            },
            Step::Run {
                description: "Switch 3 settle",
                duration: Duration::from_millis(2500),
            },
            Step::SubscribeAll {
                description: "Switch 4: 720p",
                participant: "bob",
                heights: &[720],
            },
            Step::Run {
                description: "Switch 4 settle",
                duration: Duration::from_millis(2500),
            },
            Step::SubscribeAll {
                description: "Switch 5: 180p",
                participant: "bob",
                heights: &[180],
            },
            Step::Run {
                description: "Switch 5 settle",
                duration: Duration::from_millis(2500),
            },
            Step::SubscribeAll {
                description: "Switch 6: 720p",
                participant: "bob",
                heights: &[720],
            },
            Step::Run {
                description: "Switch 6 settle",
                duration: Duration::from_millis(2500),
            },
            Step::SubscribeAll {
                description: "Switch 7: 180p",
                participant: "bob",
                heights: &[180],
            },
            Step::Run {
                description: "Switch 7 settle",
                duration: Duration::from_millis(2500),
            },
            Step::SubscribeAll {
                description: "Switch 8: 720p",
                participant: "bob",
                heights: &[720],
            },
            Step::Run {
                description: "Switch 8 settle",
                duration: Duration::from_millis(2500),
            },
            Step::SubscribeAll {
                description: "Switch 9: 180p",
                participant: "bob",
                heights: &[180],
            },
            Step::Run {
                description: "Switch 9 settle",
                duration: Duration::from_millis(2500),
            },
            Step::SubscribeAll {
                description: "Switch 10: 720p",
                participant: "bob",
                heights: &[720],
            },
            Step::Run {
                description: "Switch 10 settle",
                duration: Duration::from_millis(2500),
            },
            Step::SubscribeAll {
                description: "Switch 11: 180p",
                participant: "bob",
                heights: &[180],
            },
            Step::Run {
                description: "Switch 11 settle",
                duration: Duration::from_millis(2500),
            },
            Step::SubscribeAll {
                description: "Switch 12: 720p",
                participant: "bob",
                heights: &[720],
            },
            Step::Run {
                description: "Switch 12 settle",
                duration: Duration::from_millis(2500),
            },
            Step::Run {
                description: "Final settling period",
                duration: Duration::from_secs(3),
            },
            // Each switch is allowed to produce one sequence-number gap (truncated
            // in-flight frame). Missing parameter sets must be zero — the SFU must
            // always deliver SPS+PPS with the first keyframe after a switch.
            Step::CheckVideoQuality {
                description: "Frames remain decodable across 12 simulcast switches",
                participant: "bob",
                quality: VideoQuality::min_frames(200).allow_gaps_for_switches(12),
            },
        ]);
}
