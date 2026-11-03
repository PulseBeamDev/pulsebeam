//! Source-bound audio subscriptions and native media output.
//!
//! Libwebrtc owns encoding, transport, receive buffering, decoding and playout. These plans
//! observe its actual outputs. Public assignments identify selection separately from output,
//! never reconstructed publication identity for buffered PCM.

use super::common::{LocalNodeSim, Participant, Room, Step};
use pulsebeam_agent_native::agent_core::TopicMode;
use pulsebeam_testdata::QUALITY_AUDIO_FRAME_SAMPLES;
use std::time::Duration;

/// Audio published by a participant reaches the people in the room.
///
/// A decoded listener proves actual native PCM encoding and playout; an explicit encoded
/// listener separately observes receiver identity and
/// payloads. Sequence holes may include native padding the SFU does not forward, not missing audio.
#[test]
fn audio_reaches_the_room_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                // Separate encoded Opus and decoded PCM receivers observe the same audio-only room.
                .with_participant(Participant::data_participant("speaker").speaking_at(-30))
                .with_participant(Participant::subscriber("listener").hearing(3))
                .with_participant(Participant::subscriber("packet-listener").hearing(3).with_encoded_audio()),
        )
        .run(vec![
            Step::Run {
                description: "Speaker talks; listener should hear it",
                duration: Duration::from_secs(10),
            },
            Step::CheckAudioCatalogFrom {
                description: "The public audio assignment identifies the speaker",
                participant: "listener",
                expected: &["speaker"],
            },
            Step::CheckEncodedAudio {
                description: "Actual native encoded Opus independently decodes at the receiver",
                participant: "packet-listener",
                corrupt: false,
            },
            Step::CheckAudioSourceStreams {
                description: "The named speaker has one actual native receiver and SSRC stream",
                participant: "packet-listener",
                expected_sources: &["speaker"],
            },
            Step::CheckNativeAudioDecoded {
                description: "Actual native PCM cadence and SDK decode progress across the measured scenario",
                participant: "listener",
                min_samples: 4_800,
                min_receivers: 1,
                max_silence: Some(Duration::from_millis(1_500)),
            },
        ]);
}

#[test]
fn each_listener_must_count_only_decoded_opus_as_heard_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("decoded-audio-boundary")
                .with_participant(
                    Participant::data_participant("speaker")
                        .speaking_at(-30)
                        .with_native_pcm_audio(),
                )
                .with_participant(
                    Participant::subscriber("listener-a")
                        .hearing(1)
                        .with_native_decoded_audio(),
                )
                .with_participant(
                    Participant::subscriber("listener-b")
                        .hearing(1)
                        .with_native_decoded_audio(),
                ),
        )
        .run(vec![
            Step::Run {
                description: "Deliver the speaker to both independent listeners",
                duration: Duration::from_secs(5),
            },
            Step::CheckAudioCatalogFrom {
                description: "The catalog identifies the source separately from PCM",
                participant: "listener-a",
                expected: &["speaker"],
            },
            Step::CheckAudioCatalogFrom {
                description: "Listener B also receives the source assignment",
                participant: "listener-b",
                expected: &["speaker"],
            },
            Step::CheckNativeAudioDecoded {
                description: "Listener A has native nontrivial PCM and NetEq decode progress",
                participant: "listener-a",
                min_samples: 4_800,
                min_receivers: 1,
                max_silence: None,
            },
            Step::CheckNativeAudioDecoded {
                description: "Listener B independently has native PCM and NetEq progress",
                participant: "listener-b",
                min_samples: 4_800,
                min_receivers: 1,
                max_silence: None,
            },
        ]);
}

#[test]
fn encoded_opus_corruption_fails_independent_decoder_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("corrupt-audio-oracle")
                .with_participant(Participant::data_participant("speaker").speaking_at(-30))
                .with_participant(
                    Participant::subscriber("listener")
                        .hearing(1)
                        .with_corrupt_audio_payload(),
                ),
        )
        .run(vec![
            Step::Run {
                description: "Forward invalid Opus packets with intact RTP structure",
                duration: Duration::from_secs(5),
            },
            Step::CheckRxBytes {
                description: "The packet-only oracle sees media bytes",
                participant: "listener",
                min_bytes: 1,
            },
            Step::CheckAudioCatalogFrom {
                description: "The receiver is assigned the intended speaker before corruption",
                participant: "listener",
                expected: &["speaker"],
            },
            Step::CheckEncodedAudio {
                description: "Receiver-side corrupted Opus must fail the independent decoder",
                participant: "listener",
                corrupt: true,
            },
        ]);
}

#[test]
fn native_pcm_recovers_after_encoder_dtx_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("decoded-audio-dtx")
                .with_participant(Participant::data_participant("speaker").speaking_at(-30))
                .with_participant(Participant::subscriber("listener").hearing(1))
                .with_participant(
                    Participant::data_participant("quiet-speaker")
                        .speaking_at(-127)
                        .with_native_pcm_audio(),
                )
                .with_participant(Participant::subscriber("quiet-listener").hearing(1)),
        )
        .run(vec![
            Step::Run {
                description: "Decode native PCM before the encoder DTX interval",
                duration: Duration::from_secs(2),
            },
            Step::SetAudioIntent {
                description: "Keep the DTX recovery observation bound to the speaker",
                participant: "listener",
                pinned: &["speaker"],
                auto: false,
            },
            Step::SetAudioIntent {
                description: "Explicitly request quiet media instead of selecting a loud speaker",
                participant: "quiet-listener",
                pinned: &["quiet-speaker"],
                auto: false,
            },
            Step::Run {
                description: "Cross native encoder DTX and recover active PCM",
                duration: Duration::from_secs(4),
            },
            Step::CheckAudioCatalogFrom {
                description: "The recovered listener is assigned the speaker",
                participant: "listener",
                expected: &["speaker"],
            },
            Step::CheckNativeAudioDecoded {
                description: "Actual native PCM and SDK decode progress, without per-block source attribution",
                participant: "listener",
                min_samples: 4_800,
                min_receivers: 1,
                max_silence: None,
            },
            Step::WaitForNativeAudioRecovery {
                description: "Native PCM recovers after the second DTX region and meets the retained sample bound",
                participant: "listener",
                min_samples: (QUALITY_AUDIO_FRAME_SAMPLES * 150) as u64,
                timeout: Duration::from_secs(2),
            },
            Step::CheckAudioCatalogFrom {
                description: "The quiet publication has an explicit audio assignment",
                participant: "quiet-listener",
                expected: &["quiet-speaker"],
            },
            Step::CheckNativeAudioQuiet {
                description: "Quiet media is fresh decoded PCM, not an empty collector or concealment-only playout",
                participant: "quiet-listener",
                min_blocks: 30,
                timeout: Duration::from_secs(5),
            },
        ]);
}

#[test]
fn audio_does_not_loop_back_to_speaker_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("audio-no-loopback")
                .with_participant(
                    Participant::data_participant("speaker")
                        .speaking_at(-30)
                        .hearing(1),
                )
                .with_participant(Participant::data_participant("listener").hearing(1)),
        )
        .run(vec![
            Step::Run {
                description: "Speaker talks to the room",
                duration: Duration::from_secs(10),
            },
            Step::CheckAudioCatalogFrom {
                description: "The listener's public assignment selects the speaker",
                participant: "listener",
                expected: &["speaker"],
            },
            Step::CheckNativeAudioDecoded {
                description: "Actual native PCM and SDK decode progress, without per-block source attribution",
                participant: "listener",
                min_samples: 4_800,
                min_receivers: 1,
                max_silence: None,
            },
            Step::CheckAudioCatalogFrom {
                description: "The speaker has no self-audio assignment",
                participant: "speaker",
                expected: &[],
            },
            Step::CheckNativeAudioSilent {
                description: "No native ingress or nontrivial PCM for an excluded listener",
                participant: "speaker",
            },
        ]);
}

/// Automatic discovery includes quiet named sources rather than ranking them by loudness.
///
/// Four sources require four independent receivers. Public assignments include the quiet source;
/// the three louder sources must still produce actual nontrivial native PCM and decode progress.
#[test]
fn automatic_audio_keeps_quiet_named_sources_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::data_participant("loud").speaking_at(-25))
                .with_participant(Participant::data_participant("louder").speaking_at(-20))
                .with_participant(Participant::data_participant("loudest").speaking_at(-15))
                .with_participant(Participant::data_participant("faint").speaking_at(-75))
                .with_participant(Participant::subscriber("listener").hearing(4)),
        )
        .run(vec![
            Step::Run {
                description: "Three talk over each other while a quiet source remains subscribed",
                duration: Duration::from_secs(10),
            },
            Step::CheckAudioCatalogFrom {
                description: "All four named sources have public assignments regardless of volume",
                participant: "listener",
                expected: &["loud", "louder", "loudest", "faint"],
            },
            Step::CheckNativeAudioDecoded {
                description: "Actual native PCM and SDK decode progress, without per-block source attribution",
                participant: "listener",
                min_samples: 4_800,
                min_receivers: 3,
                max_silence: None,
            },
        ]);
}

/// The listener is told who it is hearing.
///
/// Two named speakers get two independent receivers. Public assignments identify those bindings;
/// they do not attribute buffered decoded PCM to a publisher or rank speakers by loudness.
#[test]
fn the_listener_is_told_who_it_is_hearing_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::data_participant("loud").speaking_at(-20))
                .with_participant(Participant::data_participant("quiet").speaking_at(-45))
                .with_participant(Participant::subscriber("listener").hearing(2)),
        )
        .run(vec![
            Step::Run {
                description: "Both talk, and both fit",
                duration: Duration::from_secs(10),
            },
            Step::CheckAudioCatalogFrom {
                description: "The public assignments identify both speakers",
                participant: "listener",
                expected: &["loud", "quiet"],
            },
            Step::CheckNativeAudioDecoded {
                description: "Actual native PCM and SDK decode progress, without per-block source attribution",
                participant: "listener",
                min_samples: 4_800,
                min_receivers: 2,
                max_silence: None,
            },
        ]);
}

/// Taking turns does not pool or retarget the sources' native receivers.
///
/// The same four sources keep separate actual receivers and SSRC streams throughout the original
/// speech schedule. Native PCM and independent Opus decoding separately qualify media health.
#[test]
fn taking_turns_keeps_native_audio_receivers_source_bound_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::data_participant("alice").speaking_at(-25))
                // Stagger the original 150-packet speech cycle across independent sources.
                .with_participant(
                    Participant::data_participant("bob")
                        .speaking_at(-25)
                        .taking_turns_after(40),
                )
                .with_participant(
                    Participant::data_participant("carol")
                        .speaking_at(-25)
                        .taking_turns_after(80),
                )
                .with_participant(
                    Participant::data_participant("dave")
                        .speaking_at(-25)
                        .taking_turns_after(120),
                )
                .with_participant(Participant::subscriber("listener").hearing(4))
                .with_participant(Participant::subscriber("packet-listener").hearing(4).with_encoded_audio()),
        )
        .run(vec![
            Step::Run {
                description: "Four named sources take turns on their own native receivers",
                duration: Duration::from_secs(20),
            },
            Step::CheckAudioSourceStreams {
                description: "Four public sources have four distinct actual receivers and SSRC streams",
                participant: "packet-listener",
                expected_sources: &["alice", "bob", "carol", "dave"],
            },
            Step::CheckNativeAudioDecoded {
                description: "All four independent native receivers produce nontrivial PCM and decode progress",
                participant: "listener",
                min_samples: 4_800,
                min_receivers: 4,
                max_silence: None,
            },
            Step::CheckNativeAudioDecoded {
                description: "Actual native PCM cadence and SDK decode progress across the measured scenario",
                participant: "listener",
                min_samples: 4_800,
                min_receivers: 3,
                max_silence: Some(Duration::from_millis(1_500)),
            },
            Step::CheckEncodedAudio {
                description: "Every independent encoded receiver decodes across its source's speech periods",
                participant: "packet-listener",
                corrupt: false,
            },
        ]);
}

/// A pinned speaker is heard while louder people talk over them.
///
/// The whole point of pinning, and the case automatic selection cannot serve: a listener who has
/// chosen somebody keeps hearing them without having to be the loudest thing in the room. Three
/// speakers saturate the slots at volumes the pinned one cannot compete with, so a selector that
/// only ranked by loudness would drop the pin and pass a byte count.
#[test]
fn a_pinned_speaker_is_heard_over_louder_ones_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::data_participant("quiet_pinned").speaking_at(-70))
                .with_participant(Participant::data_participant("loud_a").speaking_at(-15))
                .with_participant(Participant::data_participant("loud_b").speaking_at(-18))
                .with_participant(Participant::data_participant("loud_c").speaking_at(-20))
                .with_participant(Participant::subscriber("listener").hearing(3)),
        )
        .run(vec![
            Step::Run {
                description: "Let everyone be discovered before pinning anybody",
                duration: Duration::from_secs(5),
            },
            Step::SetAudioIntent {
                description: "The listener pins the quiet speaker",
                participant: "listener",
                pinned: &["quiet_pinned"],
                auto: true,
            },
            Step::Run {
                description: "Three louder voices compete for the remaining slots",
                duration: Duration::from_secs(15),
            },
            Step::CheckRxBytesInterval {
                description: "The listener is still receiving audio",
                participant: "listener",
                min_bytes: 1,
            },
            // Check current catalog and owned SFU assignment, not historical queueing or an
            // invented rank from receiver-index order. The quiet source cannot win on loudness.
            Step::CheckSpeakerHeld {
                description: "The quietest speaker holds a slot because it was pinned",
                participant: "listener",
                speaker: "quiet_pinned",
            },
            Step::CheckNativeAudioDecoded {
                description: "The remaining audible native receivers keep decoding while the quiet source is pinned",
                participant: "listener",
                min_samples: 4_800,
                min_receivers: 2,
                max_silence: None,
            },
        ]);
}

/// `auto: false` hears the pins and nothing else.
///
/// The difference between "prefer these" and "only these". With three free slots and three louder
/// speakers available, an implementation that treated the flag as advisory would fill them and
/// still look healthy on every byte-level measure.
#[test]
fn auto_off_hears_exactly_the_pins_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::data_participant("chosen").speaking_at(-40))
                .with_participant(Participant::data_participant("loud_a").speaking_at(-15))
                .with_participant(Participant::data_participant("loud_b").speaking_at(-18))
                .with_participant(Participant::subscriber("listener").hearing(3)),
        )
        .run(vec![
            Step::Run {
                description: "Let everyone be discovered",
                duration: Duration::from_secs(5),
            },
            Step::SetAudioIntent {
                description: "The listener asks for one speaker and no automatic fill",
                participant: "listener",
                pinned: &["chosen"],
                auto: false,
            },
            Step::Run {
                description: "The louder two keep talking into slots that stay empty",
                duration: Duration::from_secs(15),
            },
            Step::CheckAudioCatalogFrom {
                description: "Only the pinned speaker is assigned, though two slots are free",
                participant: "listener",
                expected: &["chosen"],
            },
            Step::CheckNativeAudioDecoded {
                description: "Actual native PCM and SDK decode progress, without per-block source attribution",
                participant: "listener",
                min_samples: 4_800,
                min_receivers: 1,
                max_silence: None,
            },
        ]);
}

/// Explicit track pinning excludes a competing participant.
///
/// This scenario has one audio publication per participant. It checks pin exclusion, not
/// isolation between multiple publications owned by the same participant.
#[test]
fn pinning_one_participant_does_not_select_another_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::data_participant("alice").speaking_at(-45))
                .with_participant(Participant::data_participant("bob").speaking_at(-15))
                .with_participant(Participant::subscriber("listener").hearing(1)),
        )
        .run(vec![
            Step::Run {
                description: "Let both be discovered",
                duration: Duration::from_secs(5),
            },
            Step::SetAudioIntent {
                description: "The listener pins alice's audio and nothing else",
                participant: "listener",
                pinned: &["alice"],
                auto: false,
            },
            Step::Run {
                description: "Bob talks far louder into the one slot alice holds",
                duration: Duration::from_secs(15),
            },
            Step::CheckAudioCatalogFrom {
                description: "The single public audio assignment names the pinned participant",
                participant: "listener",
                expected: &["alice"],
            },
            Step::CheckNativeAudioDecoded {
                description: "Actual native PCM and SDK decode progress, without per-block source attribution",
                participant: "listener",
                min_samples: 4_800,
                min_receivers: 1,
                max_silence: None,
            },
        ]);
}

/// A client that never mentions audio gets exactly what it got before the message existed.
///
/// The default is load bearing: every existing client sends no `AudioIntent` at all, and the
/// protocol change must be invisible to them. Stated as its own plan rather than left implied by
/// the other audio plans, because a default that quietly became `auto: false` would make all of
/// them fail together and none of them say why.
#[test]
fn saying_nothing_about_audio_keeps_automatic_selection_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::data_participant("loud").speaking_at(-20))
                .with_participant(Participant::data_participant("quiet").speaking_at(-45))
                .with_participant(Participant::subscriber("listener").hearing(2)),
        )
        .run(vec![
            Step::Run {
                description: "Nobody sets an audio intent",
                duration: Duration::from_secs(10),
            },
            Step::CheckAudioCatalogFrom {
                description: "Automatic selection assigns both available speakers",
                participant: "listener",
                expected: &["loud", "quiet"],
            },
            Step::CheckNativeAudioDecoded {
                description: "Actual native PCM and SDK decode progress, without per-block source attribution",
                participant: "listener",
                min_samples: 4_800,
                min_receivers: 2,
                max_silence: None,
            },
        ]);
}

/// Audio reaches a listener sitting on a different shard from the speaker.
///
/// Every other plan here puts the whole room on one shard, so the destination of an audio route
/// always equalled the publisher's shard and `install_audio_routes` skipped it: across the entire
/// suite, not one audio route was ever granted. The cross-shard audio path — minting a fanout key
/// on the destination, granting the route, forwarding over it — had no coverage at all, which is
/// how a video track could be granted audio routes nobody would ever notice.
///
/// Enough participants to make the placement hash split them; queued-source checks only pass if
/// audio survived the crossing.
#[test]
fn audio_crosses_a_shard_boundary_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::data_participant("speaker").speaking_at(-25))
                .with_participant(Participant::data_participant("second").speaking_at(-30))
                .with_participant(Participant::subscriber("near").hearing(3))
                .with_participant(Participant::subscriber("far").hearing(3))
                .with_participant(Participant::subscriber("further").hearing(3)),
        )
        .run(vec![
            Step::Run {
                description: "Two speakers talk; listeners are spread across both shards",
                duration: Duration::from_secs(10),
            },
            Step::CheckAudioCatalogFrom {
                description: "The public assignments identify both speakers at this listener",
                participant: "far",
                expected: &["speaker", "second"],
            },
            Step::CheckNativeAudioDecoded {
                description: "Actual native PCM and SDK decode progress, without per-block source attribution",
                participant: "far",
                min_samples: 4_800,
                min_receivers: 2,
                max_silence: None,
            },
            Step::CheckAudioCatalogFrom {
                description: "The other listener independently exposes both source assignments",
                participant: "further",
                expected: &["speaker", "second"],
            },
            Step::CheckNativeAudioDecoded {
                description: "Actual native PCM and SDK decode progress, without per-block source attribution",
                participant: "further",
                min_samples: 4_800,
                min_receivers: 2,
                max_silence: None,
            },
        ]);
}

#[test]
fn an_audio_wildcard_does_not_subscribe_to_data_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("audio-does-not-imply-data")
                .with_participant(Participant::data_participant("publisher"))
                .with_participant(Participant::subscriber("audio_listener").hearing(1)),
        )
        .run(vec![
            Step::DeclarePublishTopic {
                mode: TopicMode::Latest,
                description: "Publisher declares a data topic",
                participant: "publisher",
                topic: "events",
            },
            Step::Run {
                description: "Install the audio wildcard and publish the data track",
                duration: Duration::from_secs(3),
            },
            Step::PublishData {
                mode: TopicMode::Latest,
                description: "Publisher sends a data payload",
                participant: "publisher",
                topic: "events",
                data: b"must-stay-data-only",
            },
            Step::Run {
                description: "Allow the data publication to reconcile",
                duration: Duration::from_secs(2),
            },
            Step::CheckDataCount {
                description: "Audio-only listener receives no data payload",
                participant: "audio_listener",
                topic: "events",
                expected: 0,
            },
        ]);
}

#[test]
fn final_automatic_audio_listener_departure_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("audio-listener-departure")
                .with_participant(Participant::data_participant("speaker").speaking_at(-30))
                .with_participant(Participant::subscriber("listener").hearing(1)),
        )
        .run(vec![
            Step::Run {
                description: "Install the automatic audio listener route",
                duration: Duration::from_secs(8),
            },
            Step::CheckAudioCatalogFrom {
                description: "The listener is assigned the speaker before departure",
                participant: "listener",
                expected: &["speaker"],
            },
            Step::CheckNativeAudioDecoded {
                description: "Actual native PCM and SDK decode progress, without per-block source attribution",
                participant: "listener",
                min_samples: 4_800,
                min_receivers: 1,
                max_silence: None,
            },
            Step::Disconnect {
                description: "The final automatic audio listener leaves",
                participant: "listener",
            },
            Step::Run {
                description: "Apply participant and route retirement",
                duration: Duration::from_secs(5),
            },
            Step::CheckNotConnected {
                description: "The departed listener has no live transport",
                participant: "listener",
            },
        ]);
}
