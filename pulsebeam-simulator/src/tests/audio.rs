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

/// A room with more speakers than slots still delivers audio.
///
/// The SFU forwards only the loudest few, so a room that outnumbers a subscriber's slots puts
/// `TopNAudioSelector` in charge of who is heard. Until audio ran in simulation at all, that code
/// had only ever been exercised by its own unit tests - never against a real participant, a real
/// negotiation, or a real link. This is the first plan that makes it choose.
///
/// It asserts that somebody is heard, not *who*. Attributing received audio to a speaker needs a
/// receive path the simulated client does not have yet - it records video frames and no audio - so
/// "the loud one wins" is not yet checkable here, only that contention does not silence the room.
/// The selector's ranking itself is covered by its unit tests; what is missing is the end-to-end
/// claim, and it needs that receive path first.
#[test]
fn a_room_with_more_speakers_than_slots_still_delivers_test() {
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
            Step::CheckRxBytes {
                description: "The listener hears somebody",
                participant: "listener",
                min_bytes: 1,
            },
        ]);
}
