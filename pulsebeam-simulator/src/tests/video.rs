use super::common::{LinkProfile, LocalNodeSim, Participant, Room, Step, VideoQuality};
use std::time::Duration;

/// A publisher that attaches a synthetic L1T3 Dependency Descriptor to every
/// frame flows end-to-end and the subscriber decodes it — exercising the agent's
/// DD emission and the SFU's DD-aware forwarder (parse + Full-target forward)
/// against the egress invariants. Shedding is unit/allocation-tested; here the
/// point is that carrying DD never breaks the stream.
#[test]
fn dependency_descriptor_stream_is_forwarded_decodably_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q"]).with_temporal_dd(3))
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "Alice publishes an L1T3 DD stream; Bob subscribes and decodes",
                duration: Duration::from_secs(10),
            },
            Step::CheckVideoQuality {
                description: "Bob decodes the DD-annotated stream end to end",
                participant: "bob",
                quality: VideoQuality::min_frames(150).allow_gaps(3),
            },
        ]);
}

/// A single scalable (DD) encoding keeps delivering frames through a downlink
/// squeeze. With a minimum-height floor the slot cannot be dropped, so a broken DD
/// forwarding path would starve the subscriber — this asserts it does not. The
/// precise base-layer-degrade decision and the shed stream's decodability are
/// pinned by unit tests (`base_layer_degrade_*`, `dd_shedding_to_a_lower_target_*`);
/// this is the end-to-end smoke that carrying DD never breaks the stream under BWE
/// pressure.
#[test]
fn dependency_descriptor_stream_survives_congestion_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q"]).with_temporal_dd(3))
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "Establish flow and discover the DD track",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeToQos {
                description: "Bob keeps a floor so the slot must forward something, not drop it",
                participant: "bob",
                targets: &[("alice", 720, 180, 1)],
            },
            Step::Run {
                description: "Soak at full quality",
                duration: Duration::from_secs(5),
            },
            Step::SetBandwidth {
                description: "Squeeze the downlink so full quality no longer fits",
                participant: "bob",
                bits_per_sec: 350_000,
            },
            Step::Run {
                description: "Let the allocator degrade the scalable stream to its base layer",
                duration: Duration::from_secs(12),
            },
            Step::CheckVideoQuality {
                description: "Bob keeps receiving renderable frames through the squeeze",
                participant: "bob",
                quality: VideoQuality::min_frames(60).allow_gaps(30),
            },
        ]);
}

/// SFrame/E2EE: the publisher's payload is opaque, so the SFU's H.264 probe finds
/// no IDR, SPS, or PPS — the Dependency Descriptor is the only forwarding signal.
/// A subscribe forces a switch-up, which must replay the DD keyframe segment
/// despite the opaque payload; before the DD-native keyframe/cache path that
/// replay returned nothing and the subscriber would have been starved. This is the
/// end-to-end proof that the SFU forwards on DD alone.
#[test]
fn opaque_dependency_descriptor_stream_forwards_on_dd_alone_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q"]).with_opaque_dd(3))
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "Alice publishes an opaque (E2EE) L1T3 DD stream; Bob subscribes",
                duration: Duration::from_secs(10),
            },
            Step::CheckVideoQuality {
                description: "Bob keeps receiving forwarded frames with no readable bitstream",
                participant: "bob",
                // The payload is opaque (encrypted-like), so keyframes carry no
                // readable SPS/PPS — that is the whole point. The DD still drives
                // reassembly and keyframe detection.
                quality: VideoQuality::min_frames(120)
                    .allow_gaps(10)
                    .allow_missing_parameter_sets(u64::MAX),
            },
            Step::CheckKeyframeRequests {
                // If the encrypted stream did not forward decodably, the SFU/decoder
                // would hammer the publisher with PLIs (the fps→1 "storm" seen in
                // the browser). A healthy stream needs only a handful.
                description: "no PLI storm on the opaque stream",
                participant: "alice",
                max: 10,
            },
        ]);
}

/// The encrypted-frame path under real loss — the conditions that surfaced the
/// browser's fps→1 collapse and constant-PLI storm. The SFU forwards on the DD
/// alone (opaque payload), the subscriber reassembles from raw RTP, and both the
/// frame rate must hold and keyframe requests must stay bounded. This is the sim
/// guard for the whole class of "DD-only + encrypted" reassembly bugs.
#[test]
fn opaque_dependency_descriptor_holds_framerate_under_loss_test() {
    LocalNodeSim::new()
        .with_link(LinkProfile::cellular())
        .with_room(
            Room::new("room1")
                .with_participant(
                    Participant::publisher("alice", &["q", "h", "f"]).with_opaque_dd(3),
                )
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "Establish the opaque stream over lossy cellular",
                duration: Duration::from_secs(20),
            },
            Step::SubscribeAll {
                description: "Bob subscribes at full quality",
                participant: "bob",
                heights: &[720],
            },
            Step::Run {
                description: "Soak: frames must keep flowing, decoder must not stall",
                duration: Duration::from_secs(30),
            },
            Step::CheckVideoQuality {
                // ~30fps over 30s is ~900 frames; a collapse to a few fps would
                // fall far below this floor. Gaps are generous (cellular loss).
                description: "frame rate holds — no collapse to a crawl",
                participant: "bob",
                quality: VideoQuality::min_frames(500)
                    .allow_gaps(80)
                    .allow_missing_parameter_sets(u64::MAX),
            },
            Step::CheckKeyframeRequests {
                description: "no PLI storm even under loss",
                participant: "alice",
                max: 40,
            },
        ]);
}

/// Mixed room: a DD publisher and a marker-only subscriber that never negotiates
/// the DD extension. The SFU makes its forwarding decisions from the ingress DD and
/// the subscriber simply receives standard (possibly shed) media — DD support on the
/// receive leg is not required. Asserts the stream flows end-to-end.
#[test]
fn dd_publisher_streams_to_a_marker_only_subscriber_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q"]).with_temporal_dd(3))
                .with_participant(Participant::subscriber("bob").marker_only()),
        )
        .run(vec![
            Step::Run {
                description: "Alice publishes a DD stream; a marker-only Bob subscribes",
                duration: Duration::from_secs(10),
            },
            Step::CheckVideoQuality {
                description: "Bob decodes without negotiating DD on his receive leg",
                participant: "bob",
                quality: VideoQuality::min_frames(150).allow_gaps(3),
            },
        ]);
}

/// Mixed room: a legacy marker-only publisher (no DD) and a DD-capable subscriber.
/// With no ingress DD the SFU forwards via the marker/deep-inspection path; the
/// subscriber's DD support is simply unused. Asserts the legacy path is unaffected
/// by DD being negotiated on the receive side.
#[test]
fn marker_only_publisher_streams_to_a_dd_subscriber_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q", "h", "f"]).marker_only())
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "A marker-only Alice publishes; a DD-capable Bob subscribes",
                duration: Duration::from_secs(10),
            },
            Step::CheckVideoQuality {
                description: "Bob decodes the legacy stream end to end",
                participant: "bob",
                quality: VideoQuality::min_frames(150).allow_gaps(3),
            },
        ]);
}

#[test]
fn fast_initial_ramp_up_on_good_network_test() {
    LocalNodeSim::new()
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
                quality: VideoQuality::min_frames(200).allow_gaps(3),
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
                quality: VideoQuality::min_frames(100).allow_gaps(4),
            },
        ]);
}

// The EgressGuard backward-timestamp violation is fixed (cache monotonicity
// check + push() frontier filter).  The remaining failure is a separate
// pre-existing bug: PLI for the layer switch targets a mid/rid that no longer
// exists on the publisher side, so no fresh keyframe is delivered and bob gets
// 0 bytes throughout the soak.
#[test]
fn simulcast_stream_stability_test() {
    LocalNodeSim::new()
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
                quality: VideoQuality::min_frames(500).allow_gaps(5),
            },
        ]);
}

/// Congestion control across a shard boundary.
///
/// The rest of this suite runs single-shard, where the allocator reads a
/// publisher's measurements from a struct on its own core. Split across shards
/// those same measurements become `ShardFrame::Stats` messages and the
/// keyframe requests they provoke become reverse-lane frames, so the feedback
/// loop that degrades a stream is assembled from parts that cross a core
/// boundary and arrive late or not at all.
///
/// The property is unchanged by any of that: squeeze the downlink and the
/// subscriber keeps getting renderable frames instead of a stall.
#[test]
fn cross_shard_stream_survives_congestion_test() {
    LocalNodeSim::new()
        .with_shards(2)
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q"]).with_temporal_dd(3))
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "Establish flow across the shard boundary",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeToQos {
                description: "Bob keeps a floor so the slot must forward something",
                participant: "bob",
                targets: &[("alice", 720, 180, 1)],
            },
            Step::Run {
                description: "Soak at full quality",
                duration: Duration::from_secs(5),
            },
            Step::SetBandwidth {
                description: "Squeeze the downlink so full quality no longer fits",
                participant: "bob",
                bits_per_sec: 350_000,
            },
            Step::Run {
                description: "Let the allocator degrade on measurements sent between shards",
                duration: Duration::from_secs(12),
            },
            Step::CheckCrossShardMedia {
                description: "the stream really did cross a shard boundary",
                min_frames: 100,
            },
            Step::CheckVideoQuality {
                description: "Bob keeps receiving renderable frames through the squeeze",
                participant: "bob",
                quality: VideoQuality::min_frames(60).allow_gaps(30),
            },
        ]);
}

/// Simulcast layer switching across a shard boundary.
///
/// Switching picks a different encoding of the same track, which cross-shard
/// means the destination's fanout re-keys while packets for the previous layer
/// are still in flight on the old route. The parameter-set replay that makes a
/// switch decodable has to survive the restamp on arrival.
#[test]
fn cross_shard_simulcast_switching_stays_decodable_test() {
    LocalNodeSim::new()
        .with_shards(2)
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["f", "h", "q"]))
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "Establish flow and discover all three encodings",
                duration: Duration::from_secs(6),
            },
            Step::SetBandwidth {
                description: "Force a switch down to the lowest encoding",
                participant: "bob",
                bits_per_sec: 250_000,
            },
            Step::Run {
                description: "Settle on the low layer",
                duration: Duration::from_secs(8),
            },
            Step::SetBandwidth {
                description: "Restore headroom so the allocator switches back up",
                participant: "bob",
                bits_per_sec: 5_000_000,
            },
            Step::Run {
                description: "Settle on a higher layer",
                duration: Duration::from_secs(10),
            },
            Step::CheckCrossShardMedia {
                description: "the switching stream crossed a shard boundary",
                min_frames: 100,
            },
            Step::CheckVideoQuality {
                description: "Bob decodes across every switch",
                participant: "bob",
                quality: VideoQuality::min_frames(100).allow_gaps(30),
            },
        ]);
}
