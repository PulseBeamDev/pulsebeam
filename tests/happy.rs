use common::SimulatedNetwork;
use pulsebeam::entity::{ExternalParticipantId, ExternalRoomId, ParticipantId, RoomId};
use str0m::{Candidate, change::SdpAnswer};

mod common;

#[tokio::test]
async fn basic() {
    let (network, controller) = SimulatedNetwork::new().await;
    tokio::spawn(network.run());

    let mut alice = str0m::Rtc::new();
    alice.add_local_candidate(Candidate::host("1.1.1.1:8000".parse().unwrap(), "udp").unwrap());
    let mut alice_sdp = alice.sdp_api();
    alice_sdp.add_media(
        str0m::media::MediaKind::Video,
        str0m::media::Direction::SendOnly,
        None,
        None,
        None,
    );
    alice_sdp.add_media(
        str0m::media::MediaKind::Audio,
        str0m::media::Direction::SendOnly,
        None,
        None,
        None,
    );

    alice_sdp.add_media(
        str0m::media::MediaKind::Video,
        str0m::media::Direction::RecvOnly,
        None,
        None,
        None,
    );
    alice_sdp.add_media(
        str0m::media::MediaKind::Audio,
        str0m::media::Direction::RecvOnly,
        None,
        None,
        None,
    );
    let (offer, pending) = alice_sdp.apply().unwrap();

    let room_id = RoomId::new(ExternalRoomId::new("simulation".to_string()).unwrap());
    let participant_id =
        ParticipantId::new(ExternalParticipantId::new("alice".to_string()).unwrap());
    let answer = controller
        .allocate(room_id, participant_id, offer.to_sdp_string())
        .await
        .unwrap();

    let answer = SdpAnswer::from_sdp_string(&answer).unwrap();
    let alice_sdp = alice.sdp_api();
    alice_sdp.accept_answer(pending, answer).unwrap();
}
