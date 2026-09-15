pub(crate) mod batcher;
mod core;
mod data;
pub(crate) mod downstream;
pub mod effect;
pub(crate) mod event;
pub(crate) mod intent;
pub mod packet;
pub(crate) mod reverse;
mod signaling;
pub(crate) mod transport;
mod upstream;
#[cfg(test)]
#[path = "v1_server_acceptance_tests.rs"]
mod v1_server_integration_tests_matrix;

pub use core::*;
pub use effect::{ParticipantEffect, RoomParticipant};
pub use packet::{RoutedTrackPacket, TrackPacket, TrackPacketRef};

#[cfg(test)]
mod v1_server_integration_tests {
    use super::*;
    use crate::control::NegotiatedResources;
    use crate::control::controller::ConnectionProfile;
    use crate::entity;
    use crate::id::ShardId;
    use str0m::Rtc;
    use str0m::channel::ChannelId;

    fn participant_with_channel() -> (Participant, ChannelId) {
        let room = entity::RoomExternalId::new("v1-filter").unwrap();
        let mut rtc = Rtc::new(std::time::Instant::now());
        let cid = rtc.direct_api().create_data_channel(Default::default());
        let mut participant = Participant::new(
            ParticipantConfig {
                manual_sub: true,
                room_id: entity::RoomId::from_external(&room),
                participant_id: entity::ParticipantId::new(),
                participant_external_id: entity::ParticipantExternalId::new("alice").unwrap(),
                connection_id: entity::ConnectionId::new(),
                profile: ConnectionProfile::Native,
                initial_authorization_expiry: None,
                rtc,
                resources: NegotiatedResources::empty_for_test(),
            },
            ShardId::new(0),
            1_200,
            1_200,
        );
        participant.enable_v1_test_output("alice".to_owned(), cid);
        (participant, cid)
    }

    #[test]
    fn native_admission_emits_a_compressed_catalog_snapshot() {
        let (mut participant, _) = participant_with_channel();
        participant.stage_v1_test_output().unwrap();
        let output = participant.take_v1_test_output().unwrap();
        assert!(matches!(
            pulsebeam_proto::codec::decode_server(&output),
            Ok(pulsebeam_proto::signaling_v1::ServerMessage {
                payload: Some(
                    pulsebeam_proto::signaling_v1::server_message::Payload::Catalog(
                        pulsebeam_proto::signaling_v1::Catalog {
                            state: Some(pulsebeam_proto::signaling_v1::catalog::State::Snapshot(_)),
                            ..
                        }
                    )
                )
            })
        ));
    }
}
