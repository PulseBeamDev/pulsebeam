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
/// For a long time this could not pass at all: the SFU chose who filled a subscriber's audio slots
/// and never said who they were, so packets arrived at the agent with nothing to deliver them to.
/// `AudioAssignment` is what closed that, and removing it puts `heard from: {}` back.
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

/// The listener is told who it is hearing, and where they rank.
///
/// Audio slots are shared and stolen: the mid and the SSRC carrying a voice stay put across a
/// switch, so nothing in the media says the speaker changed. Without the assignment an
/// application can play the audio and still not know whose face to light up - which is the
/// difference between a conference call and a noise.
///
/// Two speakers and two slots, so both are heard and the ordering is the claim. The louder one
/// must be signalled as rank 0.
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
            Step::CheckHeardFrom {
                description: "Both speakers reach the listener",
                participant: "listener",
                expected: &["loud", "quiet"],
            },
            Step::CheckSpeakerRank {
                description: "The louder speaker is signalled as the loudest",
                participant: "listener",
                expected: &[("loud", 0), ("quiet", 1)],
            },
        ]);
}
