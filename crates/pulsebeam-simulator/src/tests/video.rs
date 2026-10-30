use super::common::{LinkProfile, LocalNodeSim, Participant, Room, Step, VideoQuality};
use pulsebeam_testdata::{QualityVideoLayer, QualityVideoSource};
use std::time::Duration;

fn cross_shard_video_room() -> super::common::Room {
    super::common::Room::new("cross-shard-video")
        .with_participant(super::common::Participant::single_publisher("publisher"))
        .with_participant(super::common::Participant::subscriber("subscriber"))
}

#[test]
fn every_quality_layer_has_explicit_decoded_reference_facts_test() {
    for layer in [
        QualityVideoLayer::P180,
        QualityVideoLayer::P360,
        QualityVideoLayer::P720,
    ] {
        let (width, height) = layer.dimensions();
        LocalNodeSim::new()
            .with_room(
                Room::new("decoded-quality-layers")
                    .with_participant(Participant::quality_publisher_at(
                        "quality-source",
                        QualityVideoSource::Zero,
                        layer,
                    ))
                    .with_participant(Participant::subscriber("quality-viewer")),
            )
            .run(vec![
                Step::Run {
                    description: "Deliver corpus-backed H264 on the selected layer",
                    duration: Duration::from_secs(5),
                },
                Step::CheckVideoDecodedFrom {
                    description: "Decode dimensions and bounded reference facts",
                    participant: "quality-viewer",
                    publisher: "quality-source",
                    min_frames: 20,
                    min_resolution: (width, height),
                },
            ]);
    }
}

#[test]
#[should_panic(expected = "Pre-fault frames cannot prove recovery: video did not converge")]
fn video_does_not_loop_back_to_publisher_test() {
    LocalNodeSim::new()
        .with_room(
            super::common::Room::new("video-no-loopback")
                .with_participant(Participant::publisher("publisher", &["q"]).and_subscribes())
                .with_participant(Participant::subscriber("viewer")),
        )
        .run(vec![
            Step::Run {
                description: "Establish the publisher and viewer",
                duration: Duration::from_secs(5),
            },
            Step::Run {
                description: "Forward the track to the viewer",
                duration: Duration::from_secs(5),
            },
            Step::CheckVideoQuality {
                description: "Viewer receives the publisher video",
                participant: "viewer",
                quality: VideoQuality::min_frames(50).allow_gaps(5),
            },
            Step::CheckVideoNotReceivedFrom {
                description: "Publisher receives no looped-back video",
                participant: "publisher",
                publisher: "publisher",
            },
            Step::Partition {
                description: "Stop delivery after establishing a positive decoded baseline",
                from: "viewer",
                to: "server",
            },
            Step::Run {
                description: "Drain media already in flight",
                duration: Duration::from_secs(1),
            },
            Step::WaitForVideo {
                description: "Pre-fault frames cannot prove recovery",
                participant: "viewer",
                publishers: &["publisher"],
                timeout: Duration::from_secs(3),
            },
        ]);
}

#[test]
#[should_panic(expected = "A corrupt recipient cannot prove recovery: video did not converge")]
fn each_viewer_must_report_decoded_selected_source_independently_test() {
    let (width, height) = QualityVideoLayer::P180.dimensions();
    LocalNodeSim::new()
        .with_link(LinkProfile::wifi())
        .with_room(
            Room::new("decoded-viewer-boundary")
                .with_participant(Participant::quality_publisher(
                    "source-zero",
                    QualityVideoSource::Zero,
                ))
                .with_participant(Participant::quality_publisher(
                    "source-one",
                    QualityVideoSource::One,
                ))
                .with_participant(Participant::manual_subscriber("viewer-a", 1))
                .with_participant(
                    Participant::manual_subscriber("viewer-b", 1).with_corrupt_video_payload(),
                ),
        )
        .run(vec![
            Step::Run {
                description: "Establish both sources and independent viewers",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeTo {
                description: "Viewer A selects source zero",
                participant: "viewer-a",
                targets: &[("source-zero", 180)],
            },
            Step::SubscribeTo {
                description: "Viewer B selects source zero",
                participant: "viewer-b",
                targets: &[("source-zero", 180)],
            },
            Step::Run {
                description: "Deliver the first source through loss and reordering",
                duration: Duration::from_secs(5),
            },
            Step::CheckVideoReceivedFrom {
                description: "Viewer A packet oracle sees source zero frames",
                participant: "viewer-a",
                publisher: "source-zero",
                min_frames: 20,
            },
            Step::CheckVideoReceivedFrom {
                description: "Viewer B packet oracle sees source zero frames",
                participant: "viewer-b",
                publisher: "source-zero",
                min_frames: 20,
            },
            Step::CheckVideoDecodedFrom {
                description: "Viewer A reports decoded source identity and resolution",
                participant: "viewer-a",
                publisher: "source-zero",
                min_frames: 20,
                min_resolution: (width, height),
            },
            Step::CheckVideoNotRenderedFrom {
                description: "Viewer B's corrupt receiver has no rendered progress",
                participant: "viewer-b",
                publisher: "source-zero",
            },
            Step::Disconnect {
                description: "Reset viewer A without resetting viewer B",
                participant: "viewer-a",
            },
            Step::Reconnect {
                description: "Reconnect viewer A with a fresh receiver boundary",
                participant: "viewer-a",
            },
            Step::Run {
                description: "Allow viewer A's reset receiver to settle",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeTo {
                description: "Viewer A atomically selects source one after its reset",
                participant: "viewer-a",
                targets: &[("source-one", 180)],
            },
            Step::Run {
                description: "Switch only viewer A to the second source",
                duration: Duration::from_secs(5),
            },
            Step::CheckVideoReceivedFrom {
                description: "Viewer A packet oracle sees source one frames",
                participant: "viewer-a",
                publisher: "source-one",
                min_frames: 20,
            },
            Step::CheckVideoDecodedFrom {
                description: "Viewer A reports the switched source independently",
                participant: "viewer-a",
                publisher: "source-one",
                min_frames: 20,
                min_resolution: (width, height),
            },
            Step::CheckVideoNotRenderedFrom {
                description: "Viewer B remains blank for the corrupt source zero stream",
                participant: "viewer-b",
                publisher: "source-zero",
            },
            Step::WaitForVideo {
                description: "The selected source still reaches the healthy recipient",
                participant: "viewer-a",
                publishers: &["source-one"],
                timeout: Duration::from_secs(5),
            },
            Step::WaitForVideo {
                description: "A corrupt recipient cannot prove recovery",
                participant: "viewer-b",
                publishers: &["source-zero"],
                timeout: Duration::from_secs(1),
            },
        ]);
}

#[test]
fn decoded_structurally_complete_corrupt_h264_must_not_count_as_rendered_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("corrupt-video-oracle")
                .with_participant(Participant::single_publisher("source"))
                .with_participant(Participant::subscriber("viewer").with_corrupt_video_payload()),
        )
        .run(vec![
            Step::Run {
                description: "Forward complete frame boundaries containing corrupt H264",
                duration: Duration::from_secs(5),
            },
            Step::CheckVideoReceivedFrom {
                description: "The packet-only oracle sees complete frames",
                participant: "viewer",
                publisher: "source",
                min_frames: 1,
            },
            Step::CheckVideoNotRenderedFrom {
                description: "Corrupt H264 does not count as a rendered picture",
                participant: "viewer",
                publisher: "source",
            },
        ]);
}

fn bench_participant(name: &'static str) -> Participant {
    let mut participant = Participant::single_publisher(name).and_subscribes();
    participant.slots = 7;
    participant
}

#[test]
fn remote_bench_room_joiners_render_every_other_participant() {
    LocalNodeSim::new()
        .with_room(
            Room::new("bench-room")
                .with_participant(bench_participant("alice"))
                .with_participant(Participant {
                    starts_disconnected: true,
                    ..bench_participant("bob")
                })
                .with_participant(Participant {
                    starts_disconnected: true,
                    ..bench_participant("carol")
                })
                .with_participant(Participant {
                    starts_disconnected: true,
                    ..bench_participant("dave")
                }),
        )
        .run(vec![
            Step::Run {
                description: "Establish the first remote bench participant",
                duration: Duration::from_secs(2),
            },
            Step::Join {
                description: "Bob joins within the bench join spread",
                participant: "bob",
            },
            Step::Run {
                description: "Let Bob publish and subscribe",
                duration: Duration::from_secs(1),
            },
            Step::Join {
                description: "Carol joins within the bench join spread",
                participant: "carol",
            },
            Step::Run {
                description: "Let Carol publish and subscribe",
                duration: Duration::from_secs(1),
            },
            Step::Join {
                description: "Dave joins within the bench join spread",
                participant: "dave",
            },
            Step::Run {
                description: "Converge the remote four-way bench room",
                duration: Duration::from_secs(12),
            },
            Step::CheckVideoReceivedFrom {
                description: "Alice renders Bob",
                participant: "alice",
                publisher: "bob",
                min_frames: 30,
            },
            Step::CheckVideoReceivedFrom {
                description: "Alice renders Carol",
                participant: "alice",
                publisher: "carol",
                min_frames: 30,
            },
            Step::CheckVideoReceivedFrom {
                description: "Alice renders Dave",
                participant: "alice",
                publisher: "dave",
                min_frames: 30,
            },
            Step::CheckVideoReceivedFrom {
                description: "Bob renders Alice",
                participant: "bob",
                publisher: "alice",
                min_frames: 30,
            },
            Step::CheckVideoReceivedFrom {
                description: "Bob renders Carol",
                participant: "bob",
                publisher: "carol",
                min_frames: 30,
            },
            Step::CheckVideoReceivedFrom {
                description: "Bob renders Dave",
                participant: "bob",
                publisher: "dave",
                min_frames: 30,
            },
            Step::CheckVideoReceivedFrom {
                description: "Carol renders Alice",
                participant: "carol",
                publisher: "alice",
                min_frames: 30,
            },
            Step::CheckVideoReceivedFrom {
                description: "Carol renders Bob",
                participant: "carol",
                publisher: "bob",
                min_frames: 30,
            },
            Step::CheckVideoReceivedFrom {
                description: "Carol renders Dave",
                participant: "carol",
                publisher: "dave",
                min_frames: 30,
            },
            Step::CheckVideoReceivedFrom {
                description: "Dave renders Alice",
                participant: "dave",
                publisher: "alice",
                min_frames: 30,
            },
            Step::CheckVideoReceivedFrom {
                description: "Dave renders Bob",
                participant: "dave",
                publisher: "bob",
                min_frames: 30,
            },
            Step::CheckVideoReceivedFrom {
                description: "Dave renders Carol",
                participant: "dave",
                publisher: "carol",
                min_frames: 30,
            },
        ]);
}

#[test]
/// Replays a failing run with `PULSEBEAM_SIM_SEED=<seed>` from the test output.
fn cross_shard_video_decodes_from_the_requested_source() {
    super::common::LocalNodeSim::new()
        .with_room(cross_shard_video_room())
        .run(vec![
            super::common::Step::Run {
                description: "converge publisher telemetry across the shard boundary",
                duration: Duration::from_secs(12),
            },
            super::common::Step::CheckVideoDecodedFromInterval {
                description: "the subscriber independently decodes the requested source",
                participant: "subscriber",
                publisher: "publisher",
                min_frames: 1,
                min_resolution: (1, 180),
            },
        ]);
}

/// Replays a failing run with `PULSEBEAM_SIM_SEED=<seed>` from the test output.
#[test]
fn cross_shard_keyframe_reaches_the_publisher() {
    super::common::LocalNodeSim::new()
        .with_room(
            super::common::Room::new("cross-shard-keyframe")
                .with_participant(super::common::Participant::publisher(
                    "publisher",
                    &["q", "h", "f"],
                ))
                .with_participant(super::common::Participant::subscriber("subscriber")),
        )
        .run(vec![
            super::common::Step::Run {
                description: "establish a cross-shard simulcast stream",
                duration: Duration::from_secs(8),
            },
            super::common::Step::SetBandwidth {
                description: "force a layer change that needs a fresh keyframe",
                participant: "subscriber",
                bits_per_sec: 250_000,
            },
            super::common::Step::Run {
                description: "allow the reverse feedback route to carry the request",
                duration: Duration::from_secs(8),
            },
            super::common::Step::CheckVideoDecodedFromInterval {
                description: "subscriber independently decodes the requested source across owner shards",
                participant: "subscriber",
                publisher: "publisher",
                min_frames: 1,
                min_resolution: (1, 180),
            },
            super::common::Step::CheckKeyframeRequestsAtLeast {
                description: "the publisher receives the cross-shard keyframe request",
                participant: "publisher",
                min: 1,
            },
        ]);
}

#[test]
fn stable_receiver_decodes_after_the_first_pli_response_is_lost_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("stable-pli-retry")
                .with_participant(
                    Participant::publisher("source", &["q", "h", "f"])
                        .suppress_natural_keyframe_repeats(),
                )
                .with_participant(Participant::subscriber("viewer")),
        )
        .run(vec![
            Step::Run {
                description: "Discover the multi-RID source",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeTo {
                description: "Keep one receiver on the current publisher's low layer",
                participant: "viewer",
                targets: &[("source", 180)],
            },
            Step::Run {
                description: "Establish decoded output on the selected layer",
                duration: Duration::from_secs(10),
            },
            Step::CheckVideoQualityInterval {
                description: "Recovery starts from a genuinely decoded stream",
                participant: "viewer",
                quality: VideoQuality::min_frames(1),
            },
            Step::RecoverVideoAfterLostKeyframe {
                description: "Reset decoding, send one native PLI, and discard its first q response",
                publisher: "source",
                subscriber: "viewer",
                encoding: "q",
            },
            Step::Run {
                description: "Observe server retry and the returned usable keyframe",
                duration: Duration::from_secs(5),
            },
            Step::CheckKeyframeRecovery {
                description: "Both the lost response and retry address the current local slot and RID",
                publisher: "source",
                subscriber: "viewer",
                encoding: "q",
            },
            Step::CheckVideoQualityInterval {
                description: "A usable returned keyframe restores actual decoded output",
                participant: "viewer",
                quality: VideoQuality::min_frames(1).min_keyframes(1).allow_gaps(5),
            },
        ]);
}

#[test]
fn a_pli_recovers_decoding_without_natural_keyframe_repeats_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("pli-decoded-recovery")
                .with_participant(
                    Participant::publisher("source", &["q", "h", "f"])
                        .starts_disconnected()
                        .suppress_natural_keyframe_repeats(),
                )
                .with_participant(Participant::subscriber("viewer")),
        )
        .run(vec![
            Step::Run {
                description: "Establish the viewer before the source joins",
                duration: Duration::from_secs(5),
            },
            Step::Reconnect {
                description: "Start the raw H264 source with natural keyframe repeats suppressed",
                participant: "source",
            },
            Step::Run {
                description: "Decode an initial interval before the switch",
                duration: Duration::from_secs(10),
            },
            Step::SubscribeAll {
                description: "Select the high-quality layer before the recovery test",
                participant: "viewer",
                heights: &[720],
            },
            Step::Run {
                description: "Establish the selected layer before switching",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeAll {
                description: "Switch layers and request an explicit PLI",
                participant: "viewer",
                heights: &[180],
            },
            Step::Run {
                description: "Render the recovered keyframe promptly",
                duration: Duration::from_secs(5),
            },
            Step::CheckVideoQualityInterval {
                description: "The receiver decodes continued progress after PLI recovery",
                participant: "viewer",
                quality: VideoQuality::min_frames(1).min_keyframes(1).allow_gaps(5),
            },
            Step::CheckKeyframeRequestsAtLeast {
                description: "The publisher receives the explicit PLI",
                participant: "source",
                min: 1,
            },
        ]);
}

/// Genuine stock-encoder L1T3 elementary frames pass through the native sender,
/// the real SFU and the native receiver. Require actual DD metadata, all three
/// temporal indices and independently decoded payload on the same live receiver.
/// The fixture never fabricates a dependency descriptor or reference schedule.
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
                description: "Bob decodes native L1T3 output with actual DD and temporal indices",
                participant: "bob",
                quality: VideoQuality::min_frames(150)
                    .allow_gaps(3)
                    .require_native_temporal_indices(&[0, 1, 2]),
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

/// A single L1T3 source carries native Dependency Descriptor metadata. A
/// subscribe forces a switch-up, which must replay the DD keyframe segment;
/// before the DD-native keyframe/cache path that replay returned nothing and
/// the subscriber would have been starved. Opaque-payload interpretation is
/// covered below the native sender boundary, where ciphertext is valid input.
#[test]
fn dependency_descriptor_stream_forwards_from_descriptor_metadata_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q"]).with_temporal_dd(3))
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "Alice publishes an L1T3 DD stream; Bob subscribes",
                duration: Duration::from_secs(10),
            },
            Step::CheckVideoReceivedFrom {
                description: "Bob keeps receiving frames selected from native DD metadata",
                participant: "bob",
                publisher: "alice",
                min_frames: 120,
            },
            Step::CheckKeyframeRequests {
                // If DD forwarding did not remain decodable, the SFU/decoder
                // would hammer the publisher with PLIs. A healthy stream needs
                // only a handful.
                description: "no PLI storm on the DD stream",
                participant: "alice",
                max: 10,
            },
        ]);
}

/// Native three-encoding L1T1 simulcast carries DD metadata under real loss.
/// The subscriber reassembles from raw RTP, and both frame rate and keyframe
/// requests must stay bounded. Ciphertext-specific interpretation remains
/// covered at the SFU packet/cache boundary rather than being fed illegally
/// into libwebrtc's pre-packetization H264 source.
#[test]
fn dependency_descriptor_simulcast_holds_framerate_under_loss_test() {
    LocalNodeSim::new()
        .with_link(LinkProfile::cellular())
        .with_room(
            Room::new("room1")
                .with_participant(
                    Participant::publisher("alice", &["q", "h", "f"]).with_temporal_dd(1),
                )
                .with_participant(Participant::subscriber("bob")),
        )
        .run(vec![
            Step::Run {
                description: "Establish DD simulcast over lossy cellular",
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
            Step::CheckVideoReceivedFrom {
                description: "frame rate holds — no collapse to a crawl",
                participant: "bob",
                publisher: "alice",
                min_frames: 500,
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

/// Exercise the same legacy simulcast ingress with libwebrtc's actual decoder.
/// This is separate from the encoded-capture/independent-decoder oracle above:
/// neither its frame assertions nor its currently failing result are replaced.
#[test]
fn same_receiver_encoded_and_native_decoded_video_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q", "h", "f"]))
                .with_participant(Participant::subscriber("bob").with_encoded_and_native_decoded_video()),
        )
        .run(vec![
            Step::Run {
                description: "Deliver both actual observer boundaries from the same native receiver",
                duration: Duration::from_secs(10),
            },
            Step::CheckVideoDecodedFromInterval {
                description: "Authenticated encoded AUs remain independently decodable",
                participant: "bob", publisher: "alice", min_frames: 150, min_resolution: (1280, 720),
            },
            Step::CheckNativeDecodedVideo {
                description: "Actual native decoder delivers changing fresh 720p I420 with advancing stats",
                participant: "bob", dimensions: (1280, 720), min_fresh_frames: 150,
            },
            Step::CheckNativeVideoDecodedFromInterval {
                description: "Native pixels correlate to Alice's actual current HTTP admission",
                participant: "bob", publisher: "alice", min_frames: 150, min_resolution: (1280, 720),
            },
        ]);
}

#[test]
fn marker_only_simulcast_reaches_native_decoded_video_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q", "h", "f"]).marker_only())
                .with_participant(Participant::subscriber("bob").with_native_decoded_video()),
        )
        .run(vec![
            Step::Run {
                description: "A marker-only Alice publishes to a native-decoded DD-capable Bob",
                duration: Duration::from_secs(10),
            },
            Step::CheckNativeDecodedVideo {
                description: "Native decoder delivers fresh changing 720p pixels and advancing stats",
                participant: "bob",
                dimensions: (1280, 720),
                min_fresh_frames: 150,
            },
        ]);
}

#[test]
fn fast_initial_ramp_up_on_good_network_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q", "h", "f"]))
                .with_participant(
                    Participant::subscriber("bob").with_encoded_and_native_decoded_video(),
                ),
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
            Step::CheckNativeDecodedVideo {
                description: "Bob received native-decoded 720p frames throughout ramp",
                participant: "bob",
                dimensions: (1280, 720),
                min_fresh_frames: 200,
            },
        ]);
}

#[test]
fn repeated_simulcast_switching_stays_decodable_test() {
    let mut plan = vec![
        Step::Run {
            description: "Establish native signaling and publication discovery",
            duration: Duration::from_secs(1),
        },
        Step::CheckConnected {
            description: "Publisher connected before subscription",
            participant: "alice",
        },
        Step::CheckConnected {
            description: "Subscriber connected before subscription",
            participant: "bob",
        },
        Step::SubscribeAll {
            description: "Subscribe at 720p",
            participant: "bob",
            heights: &[720],
        },
        Step::Run {
            description: "Accumulate native decoded frames at 720p",
            duration: Duration::from_secs(10),
        },
        Step::CheckNativeDecodedVideo {
            description: "Initial native decoder delivers fresh 720p pixels",
            participant: "bob",
            dimensions: (1280, 720),
            min_fresh_frames: 100,
        },
    ];
    for (description, heights, dimensions) in [
        ("Switch 1: 180p", &[180][..], (320, 180)),
        ("Switch 2: 720p", &[720][..], (1280, 720)),
        ("Switch 3: 180p", &[180][..], (320, 180)),
        ("Switch 4: 720p", &[720][..], (1280, 720)),
    ] {
        plan.extend([
            Step::SubscribeAll {
                description,
                participant: "bob",
                heights,
            },
            Step::Run {
                description: "Settle",
                duration: Duration::from_millis(2500),
            },
            Step::CheckNativeDecodedVideo {
                description: "Native playout reaches the requested rung with fresh nontrivial pixels",
                participant: "bob",
                dimensions,
                min_fresh_frames: 30,
            },
        ]);
    }
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q", "h", "f"]))
                .with_participant(Participant::subscriber("bob").with_native_decoded_video()),
        )
        .run(plan);
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
            Step::CheckVideoDecodedFromInterval {
                description: "bob independently decodes the requested source across owner shards",
                participant: "bob",
                publisher: "alice",
                min_frames: 1,
                min_resolution: (1, 180),
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
        .with_room(
            Room::new("room1")
                .with_participant(Participant::publisher("alice", &["q", "h", "f"]))
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
            Step::CheckVideoDecodedFromInterval {
                description: "bob independently decodes the requested source across owner shards",
                participant: "bob",
                publisher: "alice",
                min_frames: 1,
                min_resolution: (1, 720),
            },
            Step::CheckVideoQuality {
                description: "Bob decodes across every switch",
                participant: "bob",
                quality: VideoQuality::min_frames(100).allow_gaps(30),
            },
        ]);
}
