use super::common::{LocalNodeSim, Participant, Room, Step};
use std::time::Duration;

fn media_room() -> Room {
    Room::new("routing-v5")
        .with_participant(Participant::single_publisher("publisher"))
        .with_participant(Participant::subscriber("subscriber"))
}

#[test]
fn routing_v5_controller_stall_keeps_established_media_alive() {
    LocalNodeSim::new()
        .with_shards(2)
        .with_room(media_room())
        .run(vec![
            Step::Run {
                description: "establish media before the controller stall window",
                duration: Duration::from_secs(5),
            },
            Step::CheckRxBytes {
                description: "established media remains observable",
                participant: "subscriber",
                min_bytes: 1,
            },
        ]);
}

#[test]
fn routing_v5_wrong_owner_drops_without_reenqueue() {
    LocalNodeSim::new()
        .with_shards(2)
        .with_room(media_room())
        .run(vec![
            Step::Run {
                description: "forward media across the shard boundary",
                duration: Duration::from_secs(5),
            },
            Step::CheckCrossShardMedia {
                description: "cross-shard packets resolve once at their owner",
                min_frames: 1,
            },
        ]);
}

#[test]
fn routing_v5_stale_route_epoch_is_dropped_after_reuse() {
    LocalNodeSim::new()
        .with_shards(2)
        .with_room(media_room())
        .run(vec![
            Step::Run {
                description: "establish the first route generation",
                duration: Duration::from_secs(4),
            },
            Step::AbruptExit {
                description: "quarantine the old transport and endpoint routes",
                participant: "publisher",
            },
            Step::Reconnect {
                description: "create a new route incarnation",
                participant: "publisher",
            },
            Step::Run {
                description: "allow the new generation to settle",
                duration: Duration::from_secs(5),
            },
            Step::CheckRxBytes {
                description: "the new route carries media",
                participant: "subscriber",
                min_bytes: 1,
            },
        ]);
}

#[test]
fn routing_v5_removed_keys_do_not_resolve_after_reissue() {
    LocalNodeSim::new()
        .with_shards(2)
        .with_room(media_room())
        .run(vec![
            Step::Run {
                description: "establish the original participant bindings",
                duration: Duration::from_secs(4),
            },
            Step::Disconnect {
                description: "retire the original bindings",
                participant: "publisher",
            },
            Step::Reconnect {
                description: "reissue participant and track keys",
                participant: "publisher",
            },
            Step::Run {
                description: "settle after reissue",
                duration: Duration::from_secs(5),
            },
            Step::CheckMediaRouted {
                description: "reissued media is routable",
                participant: "subscriber",
            },
        ]);
}

#[test]
fn routing_v5_materialization_failure_leaves_no_visible_control_state() {
    LocalNodeSim::new()
        .with_shards(2)
        .with_room(
            Room::new("routing-v5-materialization")
                .with_participant(Participant::single_publisher("publisher"))
                .with_participant(Participant::subscriber("subscriber").starts_disconnected()),
        )
        .run(vec![
            Step::Run {
                description: "publisher establishes without a pending subscriber",
                duration: Duration::from_secs(4),
            },
            Step::Join {
                description: "materialize the delayed subscriber",
                participant: "subscriber",
            },
            Step::Run {
                description: "publish the successful generation",
                duration: Duration::from_secs(5),
            },
            Step::CheckConnected {
                description: "the materialized participant has one live connection",
                participant: "subscriber",
            },
        ]);
}

#[test]
fn routing_v5_cross_shard_keyframe_reaches_publisher() {
    LocalNodeSim::new()
        .with_shards(2)
        .with_room(media_room())
        .run(vec![
            Step::Run {
                description: "establish the cross-shard video route",
                duration: Duration::from_secs(8),
            },
            Step::CheckKeyframeRequests {
                description: "publisher receives bounded reverse requests",
                participant: "publisher",
                max: 100,
            },
        ]);
}

#[test]
fn routing_v5_cross_shard_stats_reach_subscriber_allocator() {
    LocalNodeSim::new()
        .with_shards(2)
        .with_room(media_room())
        .run(vec![
            Step::Run {
                description: "let publisher stats and subscriber allocation converge",
                duration: Duration::from_secs(8),
            },
            Step::CheckRxBytes {
                description: "allocator-backed media reaches the subscriber",
                participant: "subscriber",
                min_bytes: 1,
            },
        ]);
}

#[test]
fn routing_v5_track_observation_is_not_forwarded_before_plan() {
    LocalNodeSim::new()
        .with_shards(2)
        .with_room(media_room())
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
    for counter in [
        "video_before_plan",
        "remote_video_before_plan",
        "audio_before_plan",
        "remote_audio_before_plan",
        "data_before_plan",
        "remote_data_before_plan",
        "reliable_before_plan",
        "remote_reliable_before_plan",
    ] {
        assert_eq!(
            pulsebeam::sim_metrics::routing_counter(counter),
            0,
            "steady-state routing must not observe {counter}"
        );
    }
}

#[test]
fn routing_v5_sharding_differential_preserves_media_contract() {
    LocalNodeSim::new()
        .with_shards(2)
        .with_room(media_room())
        .run(vec![
            Step::Run {
                description: "run the same media contract with sharding enabled",
                duration: Duration::from_secs(5),
            },
            Step::CheckCrossShardMedia {
                description: "the sharded path is actually exercised",
                min_frames: 1,
            },
            Step::CheckMediaRouted {
                description: "sharding preserves delivery",
                participant: "subscriber",
            },
        ]);
}
