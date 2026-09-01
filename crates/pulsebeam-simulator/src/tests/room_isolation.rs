//! Room boundaries.
//!
//! A room is an isolation boundary, not a label. No state - media, data, discovery, selection or
//! routing - may be shared across one, and nothing a participant does in room A may be observable
//! from room B.
//!
//! Every other plan in this suite runs a single room, which is what let a shard-wide audio
//! selector go unnoticed: with one room, "shard-wide" and "room-wide" are the same thing and
//! nothing can tell them apart. These plans exist to tell them apart. Each gives both rooms the
//! same shape, so a leak in either direction is a failure rather than an asymmetry nobody looked
//! at.
//!
//! Topic names are deliberately identical across the two rooms. A boundary that holds only because
//! the two sides picked different names is not a boundary.

use super::common::{LocalNodeSim, Participant, Room, Step};
use std::time::Duration;

/// A participant discovers publishers in its own room and nowhere else.
///
/// The gate everything else depends on: a subscriber cannot ask for a track it was never told
/// about, so if discovery is scoped then video, audio and data have no name to cross the boundary
/// with. Asserted in both directions - a leak that only ran one way would otherwise pass here and
/// fail in production for whichever room happened to be second.
#[test]
fn discovery_does_not_cross_rooms_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room-alpha")
                .with_participant(Participant::publisher("alpha_pub", &["q"]))
                .with_participant(Participant::subscriber("alpha_sub")),
        )
        .with_room(
            Room::new("room-beta")
                .with_participant(Participant::publisher("beta_pub", &["q"]))
                .with_participant(Participant::subscriber("beta_sub")),
        )
        .run(vec![
            Step::Run {
                description: "Both rooms join and publish",
                duration: Duration::from_secs(5),
            },
            Step::CheckParticipantsKnown {
                description: "Alpha's subscriber knows only alpha's publisher",
                participant: "alpha_sub",
                expected: &["alpha_pub"],
            },
            Step::CheckParticipantsKnown {
                description: "Beta's subscriber knows only beta's publisher",
                participant: "beta_sub",
                expected: &["beta_pub"],
            },
        ]);
}

/// Video reaches its own room, and only its own room.
///
/// `SubscribeAll` asks for every track the participant has discovered, which is the strongest
/// thing a client can do to pull media toward itself. If the boundary holds, that request cannot
/// name the other room's publisher and the subscriber ends up with exactly its own room's video.
///
/// The received-bytes check is what stops this passing vacuously: a boundary that holds because
/// no video flowed at all would satisfy the discovery assertion on its own.
#[test]
fn video_does_not_cross_rooms_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room-alpha")
                .with_participant(Participant::publisher("alpha_pub", &["q"]))
                .with_participant(Participant::subscriber("alpha_sub")),
        )
        .with_room(
            Room::new("room-beta")
                .with_participant(Participant::publisher("beta_pub", &["q"]))
                .with_participant(Participant::subscriber("beta_sub")),
        )
        .run(vec![
            Step::Run {
                description: "Both rooms join and publish",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeAll {
                description: "Beta's subscriber asks for everything it can see",
                participant: "beta_sub",
                heights: &[720],
            },
            Step::Run {
                description: "Forward whatever that resolved to",
                duration: Duration::from_secs(10),
            },
            Step::CheckParticipantsKnown {
                description: "Asking for everything did not reveal the other room",
                participant: "beta_sub",
                expected: &["beta_pub"],
            },
            Step::CheckRxBytesInterval {
                description: "Beta's subscriber is receiving its own room's video",
                participant: "beta_sub",
                min_bytes: 1,
            },
        ]);
}

/// A topic name is scoped to its room, on the realtime lane.
///
/// Both rooms publish and subscribe the same topic string. An unscoped subscription - one that
/// names a topic and no publisher, so it takes every publisher on that topic - is the widest
/// selector the data plane offers and therefore the one most likely to reach past the room.
///
/// The positive and negative claims are both required. Delivery alone would pass if the boundary
/// were absent, and non-delivery alone would pass if the topic were broken.
#[test]
fn unreliable_data_does_not_cross_rooms_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room-alpha")
                .with_participant(Participant::data_participant("alpha_pub"))
                .with_participant(Participant::data_participant("alpha_sub")),
        )
        .with_room(
            Room::new("room-beta")
                .with_participant(Participant::data_participant("beta_pub"))
                .with_participant(Participant::data_participant("beta_sub")),
        )
        .run(vec![
            Step::DeclarePublishTopic {
                description: "Alpha publishes the topic",
                participant: "alpha_pub",
                topic: "shared_name",
            },
            Step::DeclarePublishTopic {
                description: "Beta publishes a topic of the same name",
                participant: "beta_pub",
                topic: "shared_name",
            },
            Step::DeclareSubscribeTopic {
                description: "Alpha subscribes unscoped - every publisher on the topic",
                participant: "alpha_sub",
                topic: "shared_name",
                scoped_to: None,
            },
            Step::DeclareSubscribeTopic {
                description: "Beta subscribes unscoped too",
                participant: "beta_sub",
                topic: "shared_name",
                scoped_to: None,
            },
            Step::Run {
                description: "Let both lanes come up",
                duration: Duration::from_millis(500),
            },
            Step::PublishData {
                description: "Alpha sends something only alpha should see",
                participant: "alpha_pub",
                topic: "shared_name",
                data: b"alpha-secret",
            },
            Step::PublishData {
                description: "Beta sends something only beta should see",
                participant: "beta_pub",
                topic: "shared_name",
                data: b"beta-secret",
            },
            Step::Run {
                description: "Deliver both",
                duration: Duration::from_millis(500),
            },
            Step::CheckDataReceived {
                description: "Alpha received its own room's payload",
                participant: "alpha_sub",
                topic: "shared_name",
                expected: b"alpha-secret",
            },
            Step::CheckDataNotReceived {
                description: "Alpha never saw beta's",
                participant: "alpha_sub",
                topic: "shared_name",
                excluded: b"beta-secret",
            },
            Step::CheckDataReceived {
                description: "Beta received its own room's payload",
                participant: "beta_sub",
                topic: "shared_name",
                expected: b"beta-secret",
            },
            Step::CheckDataNotReceived {
                description: "Beta never saw alpha's",
                participant: "beta_sub",
                topic: "shared_name",
                excluded: b"alpha-secret",
            },
        ]);
}

/// A topic name is scoped to its room, on the reliable lane.
///
/// The reliable lane resolves through a different arena and carries a reverse route the realtime
/// lane does not, so its room scoping is a separate claim rather than a corollary of the plan
/// above.
///
/// `CheckDataSequence` asserts the exact delivered sequence, so a leak from the other room shows
/// up as an extra element rather than having to be excluded by name.
#[test]
fn reliable_data_does_not_cross_rooms_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room-alpha")
                .with_participant(Participant::data_participant("alpha_pub"))
                .with_participant(Participant::data_participant("alpha_sub")),
        )
        .with_room(
            Room::new("room-beta")
                .with_participant(Participant::data_participant("beta_pub"))
                .with_participant(Participant::data_participant("beta_sub")),
        )
        .run(vec![
            Step::DeclareOrderedPublisher {
                description: "Alpha declares the ordered topic",
                participant: "alpha_pub",
                topic: "shared_name",
            },
            Step::DeclareOrderedPublisher {
                description: "Beta declares an ordered topic of the same name",
                participant: "beta_pub",
                topic: "shared_name",
            },
            Step::DeclareOrderedSubscriber {
                description: "Alpha subscribes",
                participant: "alpha_sub",
                topic: "shared_name",
            },
            Step::DeclareOrderedSubscriber {
                description: "Beta subscribes",
                participant: "beta_sub",
                topic: "shared_name",
            },
            Step::Run {
                description: "Open the ordered channels",
                duration: Duration::from_millis(500),
            },
            Step::PublishOrdered {
                description: "Alpha sends",
                participant: "alpha_pub",
                topic: "shared_name",
                data: b"alpha-1",
            },
            Step::PublishOrdered {
                description: "Beta sends",
                participant: "beta_pub",
                topic: "shared_name",
                data: b"beta-1",
            },
            Step::Run {
                description: "Deliver both",
                duration: Duration::from_millis(500),
            },
            Step::CheckDataSequence {
                description: "Alpha's sequence contains its own room's message and nothing else",
                participant: "alpha_sub",
                topic: "shared_name",
                expected: &[b"alpha-1"],
            },
            Step::CheckDataSequence {
                description: "Beta's sequence contains its own room's message and nothing else",
                participant: "beta_sub",
                topic: "shared_name",
                expected: &[b"beta-1"],
            },
        ]);
}
/// Audio selection is a per-room decision. A loud room must not silence a quiet one.
///
/// Room alpha has four speakers talking over each other, one more than `MAX_SEND_AUDIO_SLOTS`.
/// Room beta has one quiet speaker and one listener, and nothing in beta competes for anything:
/// its listener has three slots and one voice to put in them.
///
/// The routing is not what is under test - control fans audio out by room, so beta's listener has
/// a correct plan naming beta's speaker. What decides this is the selector those packets pass
/// through on the way to that plan.
///
/// Four speakers in alpha rather than three, so a steal is guaranteed: a slot's owner is never
/// made to re-contend, so with exactly three the streams that sent first would keep their slots.
/// The steal always takes the quietest slot, which at -70 dBov against -30 is beta's speaker.
///
/// The claim is about a *later* window rather than the whole run. Whether beta's speaker grabs a
/// slot during startup, before alpha has ramped up, depends on arrival order and so on the seed -
/// and a cumulative "was ever heard" check passes on the strength of that one early moment even
/// though the speaker is silenced for good a second later.
#[test]
fn audio_selection_does_not_cross_rooms_test() {
    LocalNodeSim::new()
        .with_room(
            Room::new("room-alpha")
                .with_participant(Participant::data_participant("alpha_loud").speaking_at(-30))
                .with_participant(Participant::data_participant("alpha_louder").speaking_at(-25))
                .with_participant(Participant::data_participant("alpha_evenmore").speaking_at(-20))
                .with_participant(Participant::data_participant("alpha_loudest").speaking_at(-15)),
        )
        .with_room(
            Room::new("room-beta")
                .with_participant(Participant::data_participant("beta_speaker").speaking_at(-70))
                .with_participant(Participant::subscriber("beta_listener").hearing(3)),
        )
        .run(vec![
            Step::Run {
                description: "Both rooms talk at once, until alpha has taken every slot",
                duration: Duration::from_secs(10),
            },
            Step::Run {
                description: "Steady state: alpha is saturated and stays that way",
                duration: Duration::from_secs(10),
            },
            Step::CheckRxBytesInterval {
                description: "Beta is still hearing its own speaker",
                participant: "beta_listener",
                min_bytes: 1,
            },
            Step::CheckHeardFrom {
                description: "And it is hearing the speaker from its own room",
                participant: "beta_listener",
                expected: &["beta_speaker"],
            },
        ]);
}
