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
/// **Ignored: this is a recorded defect, not a test waiting on tidying.** Audio does not reach
/// subscribers, and the reproduction is kept here because it is the only one that exists.
///
/// What is established. The agent never stamped an RFC 6464 audio level, and the SFU's selector
/// drops any audio packet arriving without one - that is fixed in this change, and the level now
/// reaches the wire. The extension is negotiated (`extmap:1 ssrc-audio-level` appears in the SDP
/// from both sides), the speaker negotiates a SendOnly audio transceiver, the listener negotiates
/// its receive slots, `AudioLooper::run` is entered, and every `send` succeeds.
///
/// What still fails. The listener receives zero bytes, the room never sees a `TrackPublished` for
/// the audio track, and the selector logs no drops - so no audio packet reaches it at all. The
/// trail ends between the agent's egress and the SFU registering the track, which is further than
/// this change goes.
///
/// Un-ignore it when audio flows; it should need no other modification.
#[ignore = "audio does not reach subscribers; see the doc comment for how far it gets"]
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
