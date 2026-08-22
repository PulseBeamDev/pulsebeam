use std::time::Duration;

#[cfg(feature = "protocol")]
use pulsebeam_proto::prelude::Message;
#[cfg(feature = "protocol")]
use pulsebeam_proto::reliable::RelDelivery;

use pulsebeam_agent_core::{
    AgentCore, ChannelKey, CoreEffect, CoreInput, MonotonicTime, RequestId, TransportGeneration,
};

#[cfg(feature = "e2ee")]
use pulsebeam_agent_core::{E2eeDirection, E2eeDomain, E2eeEpoch, E2eeKeyRing, E2eeMasterKey};
#[cfg(feature = "protocol")]
use pulsebeam_agent_core::{OrderedEvent, OrderedReceiver, TopicPublisher};

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

#[cfg(feature = "protocol")]
#[test]
fn ordered_topic_fixture_replays_in_order() {
    let mut publisher = TopicPublisher::new(7).expect("nonzero stream fixture");
    let first = publisher.publish(vec![1]).expect("first message");
    let second = publisher.publish(vec![2]).expect("second message");
    let third = publisher.publish(vec![3]).expect("third message");
    let mut receiver = OrderedReceiver::default();
    let delivery = |message: &pulsebeam_proto::reliable::RelMsg| {
        RelDelivery {
            publisher_id: String::from("alice"),
            frame: message.encode_to_vec(),
        }
        .encode_to_vec()
    };
    let first_events = receiver
        .accept_delivery(&delivery(&first))
        .expect("first delivery is decodable");
    assert!(first_events.iter().any(|event| matches!(
        event,
        OrderedEvent::Message { seq: 0, payload, .. } if payload == &[1]
    )));
    let mut events = receiver
        .accept_delivery(&delivery(&third))
        .expect("third delivery is decodable");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, OrderedEvent::Nack(_)))
    );
    events = receiver
        .accept_delivery(&delivery(&second))
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

#[cfg(feature = "e2ee")]
#[test]
fn e2ee_fixture_has_stable_frame_shape_and_round_trip() {
    let key = E2eeMasterKey::new(9, [0x42; 32]);
    let epoch = E2eeEpoch::new([0x19; 16]).expect("fixture epoch is valid");
    let domain =
        E2eeDomain::new("alice", "fixture", E2eeDirection::Send).expect("fixture domain is valid");
    let mut ring = E2eeKeyRing::new(2).expect("fixture key ring is valid");
    ring.install(key, epoch, domain.clone())
        .expect("fixture key installs");
    let mut sender = ring.encryptor(9, epoch, &domain).expect("sender is valid");
    let mut receiver = ring.receiver(9, epoch, &domain).expect("receiver is valid");
    let frame = sender.encrypt(b"conformance").expect("encryption succeeds");
    assert_eq!(frame.len(), 29 + b"conformance".len() + 16);
    assert_eq!(
        receiver.decrypt(&frame).expect("decryption succeeds"),
        b"conformance"
    );
}
