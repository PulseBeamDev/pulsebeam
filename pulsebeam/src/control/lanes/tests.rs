//! Invariants of one lane's subscriber bookkeeping.
//!
//! These run against `LaneRegistry` alone — no actor, no shard, no clock — which
//! is the point of it being a separate type.

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

/// The channel is an opaque routing handle here, so the tests stand one in for
/// `str0m::channel::ChannelId` — which has no constructor outside str0m.
type Channel = u16;
type Registry = LaneRegistry<Channel>;

fn subscriber(seed: u8) -> Subscriber<Channel> {
    Subscriber {
        key: ParticipantKey::from(slotmap::KeyData::from_ffi((1_u64 << 32) | u64::from(seed))),
        channel: u16::from(seed),
    }
}

fn wildcard_subscriber(seed: u8, on: ShardId) -> WildcardSubscriber<Channel> {
    let Subscriber { key, channel } = subscriber(seed);
    WildcardSubscriber {
        shard: on,
        key,
        channel,
    }
}

/// Publish `id` on a `Data` registry. Every test below uses that lane; the
/// lane-dependent parts are covered separately by
/// `each_lane_mints_and_actions_only_its_own_key`.
fn ready(reg: &mut Registry, id: &DataStreamId, on: ShardId) {
    let key = RuntimeStreamKey::Data(Default::default());
    reg.declare(id.clone(), on, subscriber(0).key, key);
}

fn subscribers_of(reg: &Registry, id: &DataStreamId) -> Vec<ParticipantId> {
    let mut all: Vec<_> = reg
        .get(id)
        .into_iter()
        .flat_map(|binding| binding.subscribers.values())
        .flat_map(|by_participant| by_participant.keys().copied())
        .collect();
    all.sort();
    all
}

#[test]
fn a_subscriber_that_arrives_before_the_publisher_is_delivered_when_it_appears() {
    let mut reg = Registry::new(StreamLane::Data);
    let id = stream(participant(1), "chat");

    let live = reg.subscribe(id.clone(), shard(0), participant(2), subscriber(2));
    assert_eq!(live, None, "no binding exists yet, so nothing to reconcile");
    assert!(subscribers_of(&reg, &id).is_empty());

    ready(&mut reg, &id, shard(0));

    assert_eq!(
        subscribers_of(&reg, &id),
        vec![participant(2)],
        "a parked subscriber must be attached when its stream becomes ready"
    );
}

#[test]
fn a_subscriber_that_arrives_after_the_publisher_is_attached_immediately() {
    let mut reg = Registry::new(StreamLane::Data);
    let id = stream(participant(1), "chat");
    ready(&mut reg, &id, shard(0));

    let live = reg.subscribe(id.clone(), shard(0), participant(2), subscriber(2));

    assert_eq!(live, Some(id.clone()), "a live stream owes a reconcile");
    assert_eq!(subscribers_of(&reg, &id), vec![participant(2)]);
}

#[test]
fn a_wildcard_reaches_streams_that_exist_and_streams_that_appear_later() {
    let mut reg = Registry::new(StreamLane::Data);
    let existing = stream(participant(1), "chat");
    ready(&mut reg, &existing, shard(0));

    let matched = reg.subscribe_wildcard(
        room(),
        topic("chat"),
        participant(9),
        wildcard_subscriber(9, shard(0)),
    );
    assert_eq!(matched, vec![existing.clone()]);
    assert_eq!(subscribers_of(&reg, &existing), vec![participant(9)]);

    let later = stream(participant(2), "chat");
    ready(&mut reg, &later, shard(0));
    reg.apply_wildcards(&later);

    assert_eq!(
        subscribers_of(&reg, &later),
        vec![participant(9)],
        "a wildcard must cover publishers that appear after it was made"
    );
}

#[test]
fn a_wildcard_does_not_leak_across_topics() {
    let mut reg = Registry::new(StreamLane::Data);
    let other = stream(participant(1), "telemetry");
    ready(&mut reg, &other, shard(0));

    let matched = reg.subscribe_wildcard(
        room(),
        topic("chat"),
        participant(9),
        wildcard_subscriber(9, shard(0)),
    );

    assert!(matched.is_empty());
    reg.apply_wildcards(&other);
    assert!(
        subscribers_of(&reg, &other).is_empty(),
        "a chat wildcard must not attach to a telemetry stream"
    );
}

#[test]
fn unsubscribing_a_wildcard_stops_it_reaching_future_streams() {
    let mut reg = Registry::new(StreamLane::Data);
    reg.subscribe_wildcard(
        room(),
        topic("chat"),
        participant(9),
        wildcard_subscriber(9, shard(0)),
    );
    reg.unsubscribe_wildcard(room(), topic("chat"), &participant(9));

    let later = stream(participant(2), "chat");
    ready(&mut reg, &later, shard(0));
    reg.apply_wildcards(&later);

    assert!(subscribers_of(&reg, &later).is_empty());
}

#[test]
fn unsubscribe_reports_a_change_only_when_the_live_binding_moved() {
    let mut reg = Registry::new(StreamLane::Data);
    let id = stream(participant(1), "chat");
    ready(&mut reg, &id, shard(0));
    reg.subscribe(id.clone(), shard(0), participant(2), subscriber(2));

    assert!(
        reg.unsubscribe(&id, &participant(2), true),
        "removing a live subscriber owes a reconcile"
    );
    assert!(
        !reg.unsubscribe(&id, &participant(2), true),
        "removing it twice does not"
    );
    assert!(
        !reg.unsubscribe(&id, &participant(7), true),
        "removing someone who never subscribed does not"
    );
}

#[test]
fn unsubscribing_clears_a_parked_subscriber_so_it_is_not_delivered_later() {
    let mut reg = Registry::new(StreamLane::Data);
    let id = stream(participant(1), "chat");
    reg.subscribe(id.clone(), shard(0), participant(2), subscriber(2));

    reg.unsubscribe(&id, &participant(2), true);
    ready(&mut reg, &id, shard(0));

    assert!(
        subscribers_of(&reg, &id).is_empty(),
        "a subscriber that left while parked must not be delivered when the stream appears"
    );
}

#[test]
fn retiring_a_participant_removes_it_from_live_parked_and_wildcard_state() {
    let mut reg = Registry::new(StreamLane::Data);
    let live = stream(participant(1), "chat");
    let parked = stream(participant(3), "chat");
    ready(&mut reg, &live, shard(0));
    reg.subscribe(live.clone(), shard(0), participant(9), subscriber(9));
    reg.subscribe(parked.clone(), shard(0), participant(9), subscriber(9));
    reg.subscribe_wildcard(
        room(),
        topic("telemetry"),
        participant(9),
        wildcard_subscriber(9, shard(0)),
    );

    let changed = reg.retire_participant(&participant(9));

    assert_eq!(
        changed,
        vec![live.clone()],
        "only the live binding owes a reconcile"
    );
    assert!(subscribers_of(&reg, &live).is_empty());

    ready(&mut reg, &parked, shard(0));
    assert!(
        subscribers_of(&reg, &parked).is_empty(),
        "a retired participant must not be resurrected by a late publisher"
    );

    let later = stream(participant(4), "telemetry");
    ready(&mut reg, &later, shard(0));
    reg.apply_wildcards(&later);
    assert!(
        subscribers_of(&reg, &later).is_empty(),
        "a retired participant's wildcard must not survive it"
    );
}

#[test]
fn retiring_a_participant_leaves_everyone_else_subscribed() {
    let mut reg = Registry::new(StreamLane::Data);
    let id = stream(participant(1), "chat");
    ready(&mut reg, &id, shard(0));
    reg.subscribe(id.clone(), shard(0), participant(8), subscriber(8));
    reg.subscribe(id.clone(), shard(1), participant(9), subscriber(9));

    reg.retire_participant(&participant(9));

    assert_eq!(subscribers_of(&reg, &id), vec![participant(8)]);
}

#[test]
fn subscribers_on_different_shards_are_filed_separately() {
    let mut reg = Registry::new(StreamLane::Data);
    let id = stream(participant(1), "chat");
    ready(&mut reg, &id, shard(0));
    reg.subscribe(id.clone(), shard(0), participant(8), subscriber(8));
    reg.subscribe(id.clone(), shard(1), participant(9), subscriber(9));

    let binding = reg.get(&id).expect("binding");
    assert_eq!(
        binding.subscribers.len(),
        2,
        "one entry per subscriber shard"
    );
    assert!(binding.subscribers[&shard(0)].contains_key(&participant(8)));
    assert!(binding.subscribers[&shard(1)].contains_key(&participant(9)));
}

#[test]
fn each_lane_mints_and_actions_only_its_own_key() {
    let data = Registry::new(StreamLane::Data);
    let reliable = Registry::new(StreamLane::Reliable);
    let data_key = RuntimeStreamKey::Data(Default::default());
    let reliable_key = RuntimeStreamKey::Reliable(Default::default());

    assert!(matches!(
        data.route_action(data_key),
        Some(RouteAction::Data { .. })
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
    let mut reg = Registry::new(StreamLane::Data);
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
    let mut reg = Registry::new(StreamLane::Data);
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
    let mut reg = Registry::new(StreamLane::Data);
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
