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

/// The loudest speakers are the ones forwarded.
///
/// The SFU forwards a fixed number of speakers at a time, so a room with more voices than slots
/// puts `TopNAudioSelector` in charge of who is heard. Until audio ran in simulation at all, that
/// code had only ever been exercised by its own unit tests - never against a real participant, a
/// real negotiation, or a real link.
///
/// Four speakers and three slots. Three are talking; the fourth is present and unmuted but quiet,
/// as a listener in a room is. The quiet one must be the one left out - a selector that forwarded
/// whoever arrived first would pass a byte count and fail this.
///
/// Note the four speakers: the slot count is a property of the room, not of the listener, so
/// asking to hear fewer does not create contention. An earlier version of this plan had two
/// speakers and a listener asking for one slot, and passed only because the second audio mid had
/// no send stream declared on it and silently dropped everything.
///
/// For a long time this could not pass at all: the SFU chose who filled a subscriber's audio slots
/// and never said who they were, so packets arrived at the agent with nothing to deliver them to.
/// `AudioAssignment` is what closed that, and removing it puts `heard from: {}` back.
#[test]
fn the_loudest_speakers_are_the_ones_forwarded_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::data_participant("loud").speaking_at(-25))
                .with_participant(Participant::data_participant("louder").speaking_at(-20))
                .with_participant(Participant::data_participant("loudest").speaking_at(-15))
                .with_participant(Participant::data_participant("faint").speaking_at(-75))
                .with_participant(Participant::subscriber("listener").hearing(3)),
        )
        .run(vec![
            Step::Run {
                description: "Three talk over each other; there are three slots",
                duration: Duration::from_secs(10),
            },
            Step::CheckHeardFrom {
                description: "The talkers got the slots, and the quiet one did not",
                participant: "listener",
                expected: &["loud", "louder", "loudest"],
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

/// A slot carries many speakers on one stream, and the stream stays whole.
///
/// Four people taking turns and three slots, so the selector has to steal one back and forth. The
/// slot keeps its SSRC through every steal, deliberately.
///
/// A browser cannot use a per-speaker SSRC: it has one `MediaStreamTrack` per transceiver and
/// routes by mid. Worse, libwebrtc binds a receiver's sink to the SSRC the SDP declared, so media
/// arriving on any other one is decoded and thrown away - and where no SSRC was declared, each new
/// one builds a whole receive stream with a cold NetEq, four kept per m-line, oldest destroyed.
/// Who is speaking travels in the assignment instead, which a browser can act on.
///
/// What the SFU owes in exchange is a stream that does not tear. Every speaker is rewritten onto
/// the slot's timeline, so the splice has to be seamless. When the SSRC briefly followed the
/// speaker instead, a returning voice resumed hundreds of packets ahead of where it left off, and
/// put seven seconds of apparent loss inside one stream.
#[test]
fn a_slot_carries_many_speakers_without_tearing_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room1")
                .with_participant(Participant::data_participant("alice").speaking_at(-25))
                // Staggered around the 150-packet speech cycle so somebody is always arriving to a
                // room whose slots are full, which is the only way a slot is stolen.
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
                .with_participant(Participant::subscriber("listener").hearing(3)),
        )
        .run(vec![
            Step::Run {
                description: "They take turns; three slots cannot hold four voices",
                duration: Duration::from_secs(20),
            },
            Step::CheckAudioStreams {
                description: "Three slots carried four voices, and none of them tore",
                participant: "listener",
                min_speakers: 4,
                max_streams: 3,
            },
        ]);
}
