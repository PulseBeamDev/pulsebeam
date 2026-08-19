//! Invariants of one lane's stream catalog.
//!
//! These run against `LaneRegistry` alone — no actor, no shard, no clock — which
//! is the point of it being a separate type. Who subscribes to what is no longer
//! its business; that lives in `control::patterns` and is tested there.

use super::*;

fn room_named(name: &str) -> RoomId {
    RoomId::from_external(&crate::entity::ExternalRoomId::new(name).expect("valid room id"))
}

fn room() -> RoomId {
    room_named("room")
}

fn participant(seed: u8) -> ParticipantId {
    ParticipantId::from_bytes([seed; 16])
}

fn topic(name: &str) -> Topic {
    Topic::for_test(name)
}

fn stream(publisher: ParticipantId, name: &str) -> DataStreamId {
    DataStreamId::new(room(), publisher, topic(name))
}

fn shard(index: usize) -> ShardId {
    ShardId::new(index)
}

type Registry = LaneRegistry;

/// Publish `id` on a `Data` registry. Every test below uses that lane; the
/// lane-dependent parts are covered separately by
/// `each_lane_mints_and_actions_only_its_own_key`.
fn ready(reg: &mut Registry, id: &DataStreamId, on: ShardId) {
    let key = RuntimeStreamKey::Unreliable(Default::default());
    reg.declare(id.clone(), on, Default::default(), key);
}

#[test]
fn each_lane_mints_and_actions_only_its_own_key() {
    let data = Registry::new(StreamLane::Unreliable);
    let reliable = Registry::new(StreamLane::Reliable);
    let data_key = RuntimeStreamKey::Unreliable(Default::default());
    let reliable_key = RuntimeStreamKey::Reliable(Default::default());

    assert!(matches!(
        data.route_action(data_key),
        Some(RouteAction::Unreliable { .. })
    ));
    assert!(matches!(
        reliable.route_action(reliable_key),
        Some(RouteAction::Reliable { .. })
    ));
    assert!(
        data.route_action(reliable_key).is_none(),
        "a lane must refuse the other lane's key rather than mislabel it"
    );
    assert!(reliable.route_action(data_key).is_none());
}

#[test]
fn ids_on_topic_is_scoped_to_the_room() {
    let mut reg = Registry::new(StreamLane::Unreliable);
    let here = stream(participant(1), "chat");
    ready(&mut reg, &here, shard(0));

    let elsewhere = DataStreamId::new(room_named("other"), participant(2), topic("chat"));
    ready(&mut reg, &elsewhere, shard(0));

    assert_eq!(reg.ids_on_topic(&room(), &topic("chat")), vec![here]);
}

/// The topic index exists so a wildcard is a lookup instead of a scan. It is
/// only safe while it agrees with the bindings it summarises, so every test
/// that mutates the registry checks it.
fn assert_index_agrees(reg: &Registry) {
    let mut indexed: Vec<_> = reg
        .by_topic
        .publishers
        .iter()
        .flat_map(|((room, topic), publishers)| {
            publishers
                .iter()
                .map(|publisher| DataStreamId::new(*room, *publisher, topic.clone()))
        })
        .collect();
    let mut bound: Vec<_> = reg.bindings.keys().cloned().collect();
    let sort_key = |id: &DataStreamId| (id.publisher_id, id.topic.as_ref().to_owned());
    indexed.sort_by_key(sort_key);
    bound.sort_by_key(sort_key);
    assert_eq!(indexed, bound, "topic index disagrees with the bindings");
}

#[test]
fn the_topic_index_tracks_every_declare_and_remove() {
    let mut reg = Registry::new(StreamLane::Unreliable);
    assert_index_agrees(&reg);

    let first = stream(participant(1), "chat");
    let second = stream(participant(2), "chat");
    let other = stream(participant(3), "telemetry");
    for id in [&first, &second, &other] {
        ready(&mut reg, id, shard(0));
        assert_index_agrees(&reg);
    }

    assert_eq!(reg.ids_on_topic(&room(), &topic("chat")).len(), 2);

    reg.remove(&first);
    assert_index_agrees(&reg);
    assert_eq!(
        reg.ids_on_topic(&room(), &topic("chat")),
        vec![second.clone()],
        "removing one publisher must leave the others on the topic"
    );

    reg.remove(&second);
    reg.remove(&other);
    assert_index_agrees(&reg);
    assert!(reg.ids_on_topic(&room(), &topic("chat")).is_empty());
}

#[test]
fn declaring_the_same_stream_twice_does_not_duplicate_it_on_its_topic() {
    let mut reg = Registry::new(StreamLane::Unreliable);
    let id = stream(participant(1), "chat");

    ready(&mut reg, &id, shard(0));
    ready(&mut reg, &id, shard(0));

    assert_eq!(
        reg.ids_on_topic(&room(), &topic("chat")),
        vec![id],
        "a re-declared stream must appear on its topic exactly once"
    );
    assert_index_agrees(&reg);
}
