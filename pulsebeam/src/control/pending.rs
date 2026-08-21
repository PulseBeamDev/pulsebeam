use std::collections::{HashMap, VecDeque};

use crate::{
    control::lanes::StreamLane,
    entity::{ParticipantId, TrackId},
    id::ShardId,
    keys::{DownstreamSlotKey, ParticipantKey},
    shard::router::DataStreamId,
    track::TrackMeta,
};

const MAX_BLOCKED_SUBSCRIPTIONS_PER_PARTICIPANT: usize = 64;
const MAX_ROUTE_RETRIES: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Blocked {
    AwaitingPublication,
    AwaitingRoute,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingSubscription {
    pub(crate) shard_id: ShardId,
    pub(crate) subscriber: ParticipantId,
    pub(crate) subscriber_key: ParticipantKey,
    pub(crate) slot: DownstreamSlotKey,
    pub(crate) track: TrackMeta,
    blocked: Blocked,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingStream {
    pub(crate) shard_id: ShardId,
    pub(crate) id: DataStreamId,
    pub(crate) lane: StreamLane,
}

#[derive(Debug, Default)]
pub(crate) struct PendingStreams {
    awaiting_route: VecDeque<PendingStream>,
}

impl PendingStreams {
    pub(crate) fn hold(&mut self, stream: PendingStream) {
        if self.awaiting_route.iter().any(|pending| {
            pending.shard_id == stream.shard_id
                && pending.id == stream.id
                && pending.lane == stream.lane
        }) {
            return;
        }
        self.awaiting_route.push_back(stream);
    }

    pub(crate) fn take(&mut self) -> Vec<PendingStream> {
        self.awaiting_route.drain(..).collect()
    }

    pub(crate) fn remove(&mut self, id: &DataStreamId, lane: StreamLane) {
        self.awaiting_route
            .retain(|pending| pending.id != *id || pending.lane != lane);
    }

    pub(crate) fn remove_participant(&mut self, participant: ParticipantId) {
        self.awaiting_route
            .retain(|pending| pending.id.publisher_id != participant);
    }
}

impl PendingSubscription {
    pub(crate) fn new(
        shard_id: ShardId,
        subscriber: ParticipantId,
        subscriber_key: ParticipantKey,
        slot: DownstreamSlotKey,
        track: TrackMeta,
    ) -> Self {
        Self {
            shard_id,
            subscriber,
            subscriber_key,
            slot,
            track,
            blocked: Blocked::AwaitingPublication,
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct PendingSubscriptions {
    awaiting_publication: HashMap<TrackId, Vec<PendingSubscription>>,
    awaiting_route: VecDeque<PendingSubscription>,
    counts: HashMap<ParticipantId, usize>,
}

impl PendingSubscriptions {
    pub(crate) fn hold_publication(&mut self, subscription: PendingSubscription) -> bool {
        if !self.can_hold(subscription.subscriber) {
            return false;
        }
        let track_id = subscription.track.id;
        self.retain(subscription.subscriber);
        self.awaiting_publication
            .entry(track_id)
            .or_default()
            .push(subscription);
        true
    }

    pub(crate) fn hold_route(&mut self, mut subscription: PendingSubscription) -> bool {
        if !self.can_hold(subscription.subscriber) {
            return false;
        }
        let subscriber = subscription.subscriber;
        if self.awaiting_route.len() >= MAX_ROUTE_RETRIES
            && let Some(dropped) = self.awaiting_route.pop_front()
        {
            self.release(dropped.subscriber);
        }
        subscription.blocked = Blocked::AwaitingRoute;
        self.awaiting_route.push_back(subscription);
        self.retain(subscriber);
        true
    }

    pub(crate) fn take_published(&mut self, track_id: TrackId) -> Vec<PendingSubscription> {
        let pending = self
            .awaiting_publication
            .remove(&track_id)
            .unwrap_or_default();
        for subscription in &pending {
            self.release(subscription.subscriber);
        }
        pending
    }

    pub(crate) fn take_route_retries(&mut self) -> Vec<PendingSubscription> {
        let pending = self.awaiting_route.drain(..).collect::<Vec<_>>();
        for subscription in &pending {
            self.release(subscription.subscriber);
        }
        pending
    }

    pub(crate) fn remove(
        &mut self,
        track_id: TrackId,
        subscriber: ParticipantId,
        slot: DownstreamSlotKey,
    ) {
        let mut removed = 0usize;
        if let Some(pending) = self.awaiting_publication.get_mut(&track_id) {
            pending.retain(|subscription| {
                let matches = subscription.subscriber == subscriber && subscription.slot == slot;
                removed = removed.saturating_add(usize::from(matches));
                !matches
            });
        }
        if self
            .awaiting_publication
            .get(&track_id)
            .is_some_and(Vec::is_empty)
        {
            self.awaiting_publication.remove(&track_id);
        }
        self.awaiting_route.retain(|subscription| {
            let matches = subscription.track.id == track_id
                && subscription.subscriber == subscriber
                && subscription.slot == slot;
            removed = removed.saturating_add(usize::from(matches));
            !matches
        });
        for _ in 0..removed {
            self.release(subscriber);
        }
    }

    pub(crate) fn remove_participant(&mut self, participant: ParticipantId) {
        let mut removed = 0usize;
        for pending in self.awaiting_publication.values_mut() {
            pending.retain(|subscription| {
                let matches = subscription.subscriber == participant;
                removed = removed.saturating_add(usize::from(matches));
                !matches
            });
        }
        self.awaiting_publication
            .retain(|_, pending| !pending.is_empty());
        self.awaiting_route.retain(|subscription| {
            let matches = subscription.subscriber == participant;
            removed = removed.saturating_add(usize::from(matches));
            !matches
        });
        for _ in 0..removed {
            self.release(participant);
        }
    }

    fn can_hold(&self, participant: ParticipantId) -> bool {
        self.counts.get(&participant).copied().unwrap_or_default()
            < MAX_BLOCKED_SUBSCRIPTIONS_PER_PARTICIPANT
    }

    fn retain(&mut self, participant: ParticipantId) {
        let count = self.counts.entry(participant).or_default();
        let Some(next) = count.checked_add(1) else {
            pulsebeam_runtime::fatal!("pending subscription count cannot overflow");
        };
        *count = next;
        debug_assert!(*count <= MAX_BLOCKED_SUBSCRIPTIONS_PER_PARTICIPANT);
    }

    fn release(&mut self, participant: ParticipantId) {
        let Some(count) = self.counts.get_mut(&participant) else {
            debug_assert!(false, "pending subscription count must be present");
            return;
        };
        let Some(next) = count.checked_sub(1) else {
            pulsebeam_runtime::fatal!("pending subscription count cannot underflow");
        };
        *count = next;
        if next == 0 {
            self.counts.remove(&participant);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::ExternalRoomId;

    fn participant(seed: u8) -> ParticipantId {
        ParticipantId::from_bytes([seed; 16])
    }

    fn track(seed: u8) -> TrackMeta {
        TrackMeta {
            id: participant(seed).derive_track_id(crate::entity::TrackKind::Video, "track"),
            origin: participant(seed),
            room_id: crate::entity::RoomId::from_external(
                &ExternalRoomId::new("room").expect("valid room"),
            ),
            shard_id: ShardId::new(0),
        }
    }

    fn subscription(seed: u8) -> PendingSubscription {
        PendingSubscription::new(
            ShardId::new(0),
            participant(seed),
            ParticipantKey::default(),
            DownstreamSlotKey::default(),
            track(seed),
        )
    }

    fn stream(seed: u8, lane: StreamLane) -> PendingStream {
        PendingStream {
            shard_id: ShardId::new(1),
            id: DataStreamId::new(
                track(seed).room_id,
                participant(seed),
                crate::track::Topic::for_test("topic"),
            ),
            lane,
        }
    }

    #[test]
    fn publication_release_removes_the_quota_record() {
        let mut pending = PendingSubscriptions::default();
        let item = subscription(1);
        let track_id = item.track.id;
        let participant = item.subscriber;

        assert!(pending.hold_publication(item));
        assert_eq!(pending.counts.get(&participant), Some(&1));
        assert_eq!(pending.take_published(track_id).len(), 1);
        assert!(!pending.counts.contains_key(&participant));
    }

    #[test]
    fn unsubscribe_removes_publication_and_route_waiters() {
        let mut pending = PendingSubscriptions::default();
        let item = subscription(1);
        let track_id = item.track.id;
        let participant = item.subscriber;
        let slot = item.slot;

        assert!(pending.hold_publication(item.clone()));
        assert!(pending.hold_route(item));
        pending.remove(track_id, participant, slot);

        assert!(pending.take_published(track_id).is_empty());
        assert!(pending.take_route_retries().is_empty());
        assert!(!pending.counts.contains_key(&participant));
    }

    #[test]
    fn participant_removal_cleans_all_blocked_work() {
        let mut pending = PendingSubscriptions::default();
        let item = subscription(1);
        let participant = item.subscriber;

        assert!(pending.hold_publication(item.clone()));
        assert!(pending.hold_route(item));
        pending.remove_participant(participant);

        assert!(pending.awaiting_publication.is_empty());
        assert!(pending.awaiting_route.is_empty());
        assert!(pending.counts.is_empty());
    }

    #[test]
    fn deferred_streams_are_unique_and_departures_remove_them() {
        let mut pending = PendingStreams::default();
        let item = stream(1, StreamLane::Unreliable);
        pending.hold(item.clone());
        pending.hold(item);
        pending.hold(stream(1, StreamLane::Reliable));
        pending.hold(stream(2, StreamLane::Unreliable));

        pending.remove(
            &stream(1, StreamLane::Unreliable).id,
            StreamLane::Unreliable,
        );
        assert_eq!(pending.take().len(), 2);

        pending.hold(stream(1, StreamLane::Unreliable));
        pending.remove_participant(participant(1));
        assert!(pending.take().is_empty());
    }
}
