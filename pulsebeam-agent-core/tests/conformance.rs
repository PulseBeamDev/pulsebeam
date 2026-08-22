use std::time::Duration;

use pulsebeam_agent_core::{
    AgentCore, ChannelKey, CoreEffect, CoreInput, E2eeKey, E2eeSession, MonotonicTime,
    OrderedEvent, OrderedReceiver, RequestId, TopicPublisher, TransportGeneration,
};

fn time(milliseconds: u64) -> MonotonicTime {
    MonotonicTime::from(Duration::from_millis(milliseconds))
}

#[test]
fn core_effects_are_fifo_and_generation_scoped() {
    let mut core = AgentCore::default();
    core.handle(time(0), CoreInput::Start)
        .expect("start is valid from idle");
    let generation = TransportGeneration::new(1);
    assert_eq!(core.poll_effect(), Some(CoreEffect::Connect { generation }));
    core.handle(time(1), CoreInput::TransportConnected { generation })
        .expect("the current generation can connect");
    for request_id in [1, 2] {
        core.handle(
            time(request_id),
            CoreInput::Send {
                generation,
                request_id: RequestId::new(request_id),
                channel: ChannelKey::new("v1/sys/signaling"),
                payload: vec![u8::try_from(request_id).expect("fixture id fits")],
            },
        )
        .expect("connected core accepts sends");
    }
    assert!(
        matches!(core.poll_effect(), Some(CoreEffect::Send { request_id, .. }) if request_id == RequestId::new(1))
    );
    assert!(
        matches!(core.poll_effect(), Some(CoreEffect::Send { request_id, .. }) if request_id == RequestId::new(2))
    );
    assert!(core.poll_effect().is_none());

    let stale = TransportGeneration::new(0);
    assert!(
        core.handle(time(3), CoreInput::TransportClosed { generation: stale },)
            .is_err()
    );
}

#[test]
fn reconnect_deadline_is_deterministic() {
    let mut core = AgentCore::default();
    core.handle(time(0), CoreInput::Start)
        .expect("start succeeds");
    let generation = TransportGeneration::new(1);
    core.poll_effect();
    core.handle(
        time(10),
        CoreInput::TransportFailed {
            generation,
            reason: String::from("fixture"),
        },
    )
    .expect("failure schedules reconnect");
    assert_eq!(core.next_deadline(), Some(time(1_010)));
    core.handle(time(1_009), CoreInput::Timer)
        .expect("early timer is harmless");
    assert_eq!(core.next_deadline(), Some(time(1_010)));
    core.handle(time(1_010), CoreInput::Timer)
        .expect("deadline starts the next generation");
    assert_eq!(
        core.poll_effect(),
        Some(CoreEffect::Connect {
            generation: TransportGeneration::new(2),
        })
    );
}

#[test]
fn ordered_topic_fixture_replays_in_order() {
    let mut publisher = TopicPublisher::new(7).expect("nonzero stream fixture");
    let first = publisher.publish(vec![1]).expect("first message");
    let second = publisher.publish(vec![2]).expect("second message");
    let third = publisher.publish(vec![3]).expect("third message");
    let mut receiver = OrderedReceiver::default();
    let first_events = receiver
        .accept_delivery(&publisher.encode_delivery("alice", &first))
        .expect("first delivery is decodable");
    assert!(first_events.iter().any(|event| matches!(
        event,
        OrderedEvent::Message { seq: 0, payload, .. } if payload == &[1]
    )));
    let mut events = receiver
        .accept_delivery(&publisher.encode_delivery("alice", &third))
        .expect("third delivery is decodable");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, OrderedEvent::Nack(_)))
    );
    events = receiver
        .accept_delivery(&publisher.encode_delivery("alice", &second))
        .expect("second delivery is decodable");
    assert!(events.iter().any(|event| matches!(
        event,
        OrderedEvent::Message { seq: 1, payload, .. } if payload == &[2]
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        OrderedEvent::Message { seq: 2, payload, .. } if payload == &[3]
    )));
}

#[test]
fn e2ee_fixture_has_stable_frame_shape_and_round_trip() {
    let key = E2eeKey::new(9, [0x42; 32]);
    let mut sender = E2eeSession::new(key.clone()).expect("fixture key is valid");
    let mut receiver = E2eeSession::new(key).expect("fixture key is valid");
    let frame = sender.encrypt(b"conformance").expect("encryption succeeds");
    assert_eq!(frame.len(), 13 + b"conformance".len() + 16);
    assert_eq!(
        receiver.decrypt(&frame).expect("decryption succeeds"),
        b"conformance"
    );
}
