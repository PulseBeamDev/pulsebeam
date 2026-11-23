use super::*;
use crate::media::{ChannelConfig, Reliability};
use crate::participant::event::test_utils::MockParticipantSink;

#[test]
fn unowned_topic_channels_cannot_create_routes() {
    let mut connection = Connection::new(
        ConnectionConfig {
            scope: crate::entity::ProjectRoomKey::fixture(entity::RoomId::new("topics").unwrap())
                .into(),
            participant_id: crate::entity::fixture_participant_id(),
            connection_id: entity::ConnectionId::new(),
            profile: SignalingProtocol::Whip,
            authorization_expiry: None,
            rtc: crate::control::application_only_test_connection(),
            resources: NegotiatedResources::empty_for_test(),
        },
        ShardId::new(0),
        8,
    );
    let mut sink = MockParticipantSink::new();
    let now = Instant::now();
    for (id, label) in [(1, "v2/latest/pub/chat"), (2, "v2/latest/sub/chat")] {
        let channel = crate::media::ChannelId::from_value(id);
        connection.handle_event(
            now,
            Event::DataChannel(DataChannelEvent::Opened {
                channel,
                config: ChannelConfig {
                    label: label.into(),
                    ordered: false,
                    reliability: Reliability::MaxRetransmits(0),
                    negotiated: false,
                    id: None,
                    priority: pulsebeam_rtc::DataChannelPriority::MEDIUM,
                    protocol: "".into(),
                },
            }),
            &mut sink,
        );
        assert!(!connection.owns_data_channel(channel));
    }
    assert!(sink.publish_track_calls.is_empty());
    assert!(sink.owned_subscriptions.is_empty());
    assert!(connection.disconnect_reason.is_none());
}
