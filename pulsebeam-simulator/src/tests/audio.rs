#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)] // test / simulation support
//! Audio forwarding and speaker selection.
//!
//! The SFU forwards a limited number of speakers at a time, ranked by loudness, so audio has a
//! selection stage video does not. `TopNAudioSelector` is unit tested; until these plans existed
//! none of it had ever run against a real participant, and no simulated participant had ever
//! published audio at all.

use super::common::{LocalNodeSim, Participant, Room, Step};
use std::time::Duration;

/// Audio published by a participant reaches the people in the room.
///
/// The weakest possible claim about audio, and it did not hold. The SFU ranks speakers by RFC 6464
/// loudness and drops any packet arriving without a level, and the agent never stamped one - so
/// every audio packet a client published was discarded at the selector. Removing the stamp again
/// puts 499 drops in a ten-second run and fails this plan.
#[test]
fn audio_reaches_the_room_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                // Audio only, so the byte check below cannot be satisfied by video.
                .with_participant(Participant::data_participant("speaker").speaking_at(-30))
                .with_participant(Participant::subscriber("listener").hearing(3)),
        )
        .run(vec![
            Step::Run {
                description: "Speaker talks; listener should hear it",
                duration: Duration::from_secs(10),
            },
            Step::CheckRxBytes {
                description: "The listener received media",
                participant: "listener",
                min_bytes: 1,
            },
        ]);
}

/// The loudest speaker is the one forwarded.
///
/// The SFU forwards only the loudest few, so a room that outnumbers a subscriber's slots puts
/// `TopNAudioSelector` in charge of who is heard. Until audio ran in simulation at all, that code
/// had only ever been exercised by its own unit tests - never against a real participant, a real
/// negotiation, or a real link. This is the first plan that makes it choose.
///
/// One slot and two speakers, one of them 50dB louder. The quiet one is present and unmuted, as a
/// listener in a room is; the loud one is talking. The listener must hear the talker and only the
/// talker - a selector that forwarded whoever arrived first would pass a byte count and fail this.
///
/// **Ignored: a recorded defect.** It fails with `heard from: {}` because an application cannot
/// receive audio at all, which is the other half of the audio bug.
///
/// Sending now works: the level is stamped, the SFU accepts and forwards, and the listener's
/// transport receives ~50kB. But the agent hands incoming RTP to an application through
/// `media_targets`, keyed by mid and populated *only* from `assignments_upsert` - and the protocol
/// has one assignment type, `VideoAssignment`. There is no audio assignment anywhere in
/// `signaling.proto`, and no audio handling in the driver. So the SFU chooses which speakers fill
/// a subscriber's audio slots and never says who they are, and the packets arrive with nothing to
/// deliver them to.
///
/// Closing that needs an assignment carrying mid -> speaker, the SFU emitting it as the selector
/// switches, and the agent surfacing a `RemoteTrack` per audio slot. The receive path here is
/// already written for it and will work unchanged once those exist.
#[ignore = "an application cannot receive audio: the protocol has no audio assignment"]
#[test]
fn the_loudest_speaker_is_the_one_forwarded_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::data_participant("loud").speaking_at(-25))
                .with_participant(Participant::data_participant("faint").speaking_at(-75))
                .with_participant(Participant::subscriber("listener").hearing(1)),
        )
        .run(vec![
            Step::Run {
                description: "Both talk; only one slot is available",
                duration: Duration::from_secs(10),
            },
            Step::CheckHeardFrom {
                description: "The talker got the slot, and the quiet one did not",
                participant: "listener",
                expected: &["loud"],
            },
        ]);
}
