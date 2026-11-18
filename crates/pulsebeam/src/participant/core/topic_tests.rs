use super::*;
use crate::media::{ChannelConfig, Reliability};
use crate::participant::event::test_utils::MockParticipantSink;
use crate::participant::policy::TopicPublications;

fn connection(publications: TopicPublications) -> Connection {
    Connection::new(
        ConnectionConfig {
            manual_sub: false,
            room_id: entity::RoomId::from_external(&entity::RoomExternalId::new("topics").unwrap()),
            participant_id: entity::ParticipantId::new(),
            participant_external_id: entity::ParticipantExternalId::new("peer").unwrap(),
            connection_id: entity::ConnectionId::new(),
            profile: SignalingProtocol::Whip,
            allow_topic_channels: true,
            topic_publications: publications,
            authorization_expiry: None,
            rtc: crate::control::application_only_test_connection(),
            resources: NegotiatedResources::empty_for_test(),
        },
        ShardId::new(0),
        8,
    )
}

fn opened(channel: u16, label: &str) -> Event {
    Event::DataChannel(DataChannelEvent::Opened {
        channel: crate::media::ChannelId::from_value(channel),
        config: ChannelConfig {
            label: label.into(),
            ordered: false,
            reliability: Reliability::MaxRetransmits(0),
            negotiated: false,
            id: None,
            priority: pulsebeam_rtc::DataChannelPriority::MEDIUM,
            protocol: "".into(),
        },
    })
}

#[test]
fn publication_admission_precedes_reservations_and_preserves_other_channels() {
    let mut connection = connection(TopicPublications::Labels(vec!["v2/latest/chat".into()]));
    let mut sink = MockParticipantSink::new();
    let now = Instant::now();
    let first = crate::media::ChannelId::from_value(1);
    connection.handle_event(now, opened(1, "v2/latest/pub/chat"), &mut sink);
    let original = connection.data.publication(first).unwrap().clone();
    assert_eq!(original.label.as_deref(), Some("v2/latest/chat"));
    assert_eq!(sink.publish_track_calls, vec![original.id]);

    for label in ["v2/ordered/pub/chat", "v2/latest/pub/Chat"]
        .into_iter()
        .chain(std::iter::repeat_n(
            "v2/latest/pub/blocked",
            crate::track::MAX_DATA_TOPIC_CHANNELS + 1,
        ))
    {
        connection.handle_event(now, opened(2, label), &mut sink);
        assert!(
            connection
                .data
                .channel(crate::media::ChannelId::from_value(2))
                .is_none()
        );
        assert!(connection.disconnect_reason.is_none());
        assert_eq!(connection.data.publication(first), Some(&original));
        assert_eq!(sink.publish_track_calls, vec![original.id]);
        assert!(sink.unpublish_track_calls.is_empty());
    }

    // Publication authority is not subscription authority; actual receipt is source-gated in control.
    connection.handle_event(now, opened(4, "v2/latest/sub/blocked"), &mut sink);
    let subscriber = crate::media::ChannelId::from_value(4);
    assert!(connection.data.channel(subscriber).is_some());
    assert!(connection.data.publication(subscriber).is_none());
    connection.handle_event(
        now,
        Event::DataChannel(DataChannelEvent::Closed { channel: first }),
        &mut sink,
    );
    assert_eq!(sink.unpublish_track_calls, vec![original.id]);
    assert!(connection.data.channel(subscriber).is_some());
    connection.handle_event(now, opened(3, "v2/latest/pub/chat"), &mut sink);
    let reopened = connection
        .data
        .publication(crate::media::ChannelId::from_value(3))
        .unwrap();
    assert_eq!(
        reopened.id,
        connection
            .connection_id
            .derive_track_id(TrackKind::Data, "v2/latest/chat/2")
    );
    assert_ne!(reopened.id, original.id);
    assert_eq!(reopened.connection_id, original.connection_id);
    assert_eq!(sink.publish_track_calls, vec![original.id, reopened.id]);
    assert!(connection.disconnect_reason.is_none());
}

#[test]
fn expired_authority_cannot_admit_topic_publications_or_subscription_intent() {
    let mut connection = connection(TopicPublications::All);
    connection.authorization_expiry =
        Some(pulsebeam_core::auth::AuthorizationExpiry::from_unix_seconds(0));
    let mut sink = MockParticipantSink::new();
    let now = Instant::now();
    for (id, label) in [(1, "v2/latest/pub/chat"), (2, "v2/latest/sub/chat")] {
        connection.handle_event(now, opened(id, label), &mut sink);
        assert!(
            connection
                .data
                .channel(crate::media::ChannelId::from_value(id))
                .is_none()
        );
        assert!(connection.disconnect_reason.is_none());
    }
    assert!(sink.publish_track_calls.is_empty());
    assert!(sink.unpublish_track_calls.is_empty());
    connection.authorization_expiry = None;
    connection.handle_event(now, opened(3, "v2/latest/pub/chat"), &mut sink);
    assert_eq!(sink.publish_track_calls.len(), 1);
    assert_eq!(
        sink.publish_track_calls[0],
        connection
            .connection_id
            .derive_track_id(TrackKind::Data, "v2/latest/chat/1")
    );
}
