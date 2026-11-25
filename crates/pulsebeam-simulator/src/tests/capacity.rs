use std::{sync::Arc, time::Duration};

use pulsebeam_rtc_simulation::NativeH264FixtureEncoding;

use super::common::{
    harness::{LinkProfile, LocalNodeSim, Participant, Room, Step},
    media::NativeVideoFixture,
};

#[test]
fn sparse_rooms_measure_fresh_decode_during_viewer_churn() {
    const FIRST: &[(&str, &str)] = &[("viewer-a", "camera-a"), ("viewer-b", "camera-b")];
    const NEXT: &[(&str, &str)] = &[("next-a", "camera-a"), ("next-b", "camera-b")];
    const TIMEOUT: Duration = Duration::from_secs(1);
    let fixture = Arc::new(
        NativeVideoFixture::stock_temporal(
            &[(
                "q",
                NativeH264FixtureEncoding {
                    width: 320,
                    height: 180,
                    bitrate_kbps: 200,
                },
            )],
            1,
            30,
        )
        .unwrap(),
    );
    let mut sim = LocalNodeSim::new().with_link(LinkProfile::fiber());
    for (room, camera, first, next) in [
        ("active-a", "camera-a", "viewer-a", "next-a"),
        ("active-b", "camera-b", "viewer-b", "next-b"),
    ] {
        sim = sim.with_room(
            Room::new(room)
                .with_participant(
                    Participant::publisher(camera, &["q"])
                        .with_native_video_fixture(fixture.clone()),
                )
                .with_participant(
                    Participant::multi_subscriber(first, 1)
                        .with_native_playback()
                        .starts_disconnected(),
                )
                .with_participant(
                    Participant::multi_subscriber(next, 1)
                        .with_native_playback()
                        .starts_disconnected(),
                ),
        );
    }
    for name in [
        "quiet-a", "quiet-b", "quiet-c", "quiet-d", "quiet-e", "quiet-f", "quiet-g", "quiet-h",
    ] {
        sim = sim.with_room(Room::new(name).with_participant(Participant::data_participant(name)));
    }

    let mut plan = Vec::new();
    for (viewers, description) in [
        (FIRST, "initial viewer cohort"),
        (NEXT, "replacement viewer cohort"),
    ] {
        if viewers == NEXT {
            plan.extend(FIRST.iter().map(|&(participant, _)| Step::Disconnect {
                description: "Depart the previous viewer cohort while publishers continue",
                participant,
            }));
        }
        plan.extend(viewers.iter().map(|&(participant, _)| Step::Join {
            description,
            participant,
        }));
        plan.push(Step::MeasureJoinToDecode {
            description,
            viewers,
            timeout: TIMEOUT,
        });
    }
    sim.run(plan);
}
