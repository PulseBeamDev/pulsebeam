use super::common::{LocalNodeSim, Participant, Room, Step, VideoQuality};
use std::time::Duration;

/// A simulcast sender on an unimpaired network must reach link capacity
/// almost immediately and never collapse back to base-layer afterwards.
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
            // Ramp window: must already be at ≥400kbps within 2 seconds.
            // 2s × 50_000 B/s = 100_000 bytes minimum.
            Step::Run {
                description: "2s ramp window",
                duration: Duration::from_secs(2),
            },
            Step::CheckTxBytesInterval {
                description: "Alice reached ≥400kbps within 2s",
                participant: "alice",
                min_bytes: 100_000,
            },
            // Soak: floor must hold at ≥250kbps for 18 more seconds.
            // 18s × 31_250 B/s = 562_500 bytes minimum.
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
