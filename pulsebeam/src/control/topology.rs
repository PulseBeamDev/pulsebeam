use std::collections::{HashMap, HashSet};

use slotmap::{SlotMap, new_key_type};
use tokio::time::Instant;

use crate::{
    entity::{ParticipantId, RoomId, TrackId, TrackKind},
    id::ShardId,
    route::{PackedRoute, RouteError, RouteHandle, RouteId, SlotAllocator},
    track::Track,
};

new_key_type! {
    pub(crate) struct SubscriptionId;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TrackIdentity {
    pub room_id: RoomId,
    pub publisher: ParticipantId,
    pub id: TrackId,
}

impl TrackIdentity {
    pub(crate) fn from_track(track: &Track) -> Self {
        Self {
            room_id: track.meta().room_id,
            publisher: track.meta().origin,
            id: track.id(),
        }
    }

    pub(crate) fn kind(self) -> TrackKind {
        self.id.kind()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum TrackSelector {
    Exact(TrackIdentity),
    RoomKind {
        room_id: RoomId,
        kind: TrackKind,
    },
    PublisherKind {
        room_id: RoomId,
        publisher: ParticipantId,
        kind: TrackKind,
    },
}

impl TrackSelector {
    fn matches(self, track: TrackIdentity) -> bool {
        match self {
            Self::Exact(identity) => identity == track,
            Self::RoomKind { room_id, kind } => track.room_id == room_id && track.kind() == kind,
            Self::PublisherKind {
                room_id,
                publisher,
                kind,
            } => track.room_id == room_id && track.publisher == publisher && track.kind() == kind,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Subscription {
    pub id: SubscriptionId,
    pub subscriber: ParticipantId,
    pub selector: TrackSelector,
}

#[derive(Debug, Default)]
pub(crate) struct TrackTopology {
    tracks: HashMap<TrackIdentity, Track>,
    subscriptions: SlotMap<SubscriptionId, Subscription>,
    by_room_kind: HashMap<(RoomId, TrackKind), HashSet<SubscriptionId>>,
}

impl TrackTopology {
    pub(crate) fn publish(&mut self, track: Track) -> Option<TrackIdentity> {
        let identity = TrackIdentity::from_track(&track);
        if self.tracks.insert(identity, track).is_some() {
            debug_assert!(false, "a track identity must be published once");
            return None;
        }
        Some(identity)
    }

    pub(crate) fn unpublish(&mut self, identity: TrackIdentity) -> Option<Track> {
        self.tracks.remove(&identity)
    }

    pub(crate) fn track(&self, identity: TrackIdentity) -> Option<&Track> {
        self.tracks.get(&identity)
    }

    pub(crate) fn track_mut(&mut self, identity: TrackIdentity) -> Option<&mut Track> {
        self.tracks.get_mut(&identity)
    }

    pub(crate) fn subscribe(
        &mut self,
        subscriber: ParticipantId,
        selector: TrackSelector,
    ) -> SubscriptionId {
        let id = self.subscriptions.insert_with_key(|id| Subscription {
            id,
            subscriber,
            selector,
        });
        let (room_id, kind) = match selector {
            TrackSelector::Exact(identity) => (identity.room_id, identity.kind()),
            TrackSelector::RoomKind { room_id, kind }
            | TrackSelector::PublisherKind { room_id, kind, .. } => (room_id, kind),
        };
        let inserted = self
            .by_room_kind
            .entry((room_id, kind))
            .or_default()
            .insert(id);
        debug_assert!(inserted);
        id
    }

    pub(crate) fn unsubscribe_matching(
        &mut self,
        subscriber: ParticipantId,
        selector: TrackSelector,
    ) -> bool {
        let id = self.subscriptions.iter().find_map(|(id, subscription)| {
            (subscription.subscriber == subscriber && subscription.selector == selector)
                .then_some(id)
        });
        let Some(id) = id else { return false };
        self.remove_subscription(id)
    }

    fn remove_subscription(&mut self, id: SubscriptionId) -> bool {
        let Some(subscription) = self.subscriptions.remove(id) else {
            debug_assert!(false, "subscription index must contain its slot");
            return false;
        };
        let (room_id, kind) = match subscription.selector {
            TrackSelector::Exact(identity) => (identity.room_id, identity.kind()),
            TrackSelector::RoomKind { room_id, kind }
            | TrackSelector::PublisherKind { room_id, kind, .. } => (room_id, kind),
        };
        if let Some(ids) = self.by_room_kind.get_mut(&(room_id, kind)) {
            let removed = ids.remove(&id);
            debug_assert!(removed);
            if ids.is_empty() {
                self.by_room_kind.remove(&(room_id, kind));
            }
        }
        true
    }

    pub(crate) fn unsubscribe(&mut self, id: SubscriptionId) -> Option<Subscription> {
        let subscription = self.subscriptions.remove(id)?;
        let (room_id, kind) = match subscription.selector {
            TrackSelector::Exact(identity) => (identity.room_id, identity.kind()),
            TrackSelector::RoomKind { room_id, kind }
            | TrackSelector::PublisherKind { room_id, kind, .. } => (room_id, kind),
        };
        let Some(index) = self.by_room_kind.get_mut(&(room_id, kind)) else {
            debug_assert!(false, "subscription index must contain every subscription");
            return Some(subscription);
        };
        debug_assert!(index.remove(&id));
        if index.is_empty() {
            self.by_room_kind.remove(&(room_id, kind));
        }
        Some(subscription)
    }

    pub(crate) fn matches(
        &self,
        identity: TrackIdentity,
    ) -> impl Iterator<Item = Subscription> + '_ {
        self.by_room_kind
            .get(&(identity.room_id, identity.kind()))
            .into_iter()
            .flat_map(|ids| ids.iter())
            .filter_map(|id| self.subscriptions.get(*id))
            .copied()
            .filter(move |subscription| subscription.selector.matches(identity))
    }

    pub(crate) fn contains(&self, identity: TrackIdentity) -> bool {
        self.tracks.contains_key(&identity)
    }

    pub(crate) fn tracks_in_room(
        &self,
        room_id: RoomId,
        kind: TrackKind,
    ) -> impl Iterator<Item = TrackIdentity> + '_ {
        self.tracks
            .keys()
            .filter(move |identity| identity.room_id == room_id && identity.kind() == kind)
            .copied()
    }

    pub(crate) fn identities(&self) -> impl Iterator<Item = TrackIdentity> + '_ {
        self.tracks.keys().copied()
    }

    pub(crate) fn remove_participant(&mut self, participant: ParticipantId) {
        let subscriptions: Vec<_> = self
            .subscriptions
            .iter()
            .filter_map(|(id, subscription)| (subscription.subscriber == participant).then_some(id))
            .collect();
        for id in subscriptions {
            let _ = self.unsubscribe(id);
        }
        self.tracks
            .retain(|identity, _| identity.publisher != participant);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TrackAllocation {
    pub key: crate::keys::TrackKey,
    pub route: RouteHandle,
}

#[derive(Debug)]
pub(crate) struct TrackAllocator {
    keys: SlotMap<crate::keys::TrackKey, TrackIdentity>,
    routes: Vec<SlotAllocator>,
}

impl TrackAllocator {
    pub(crate) fn new(shard_count: usize) -> Self {
        Self {
            keys: SlotMap::with_key(),
            routes: (0..shard_count)
                .map(|index| {
                    SlotAllocator::with_max_slots(
                        ShardId::new(index),
                        PackedRoute::MAX_SLOT.saturating_add(1),
                    )
                })
                .collect(),
        }
    }

    pub(crate) fn allocate(
        &mut self,
        shard: ShardId,
        identity: TrackIdentity,
        now: Instant,
    ) -> Result<TrackAllocation, RouteError> {
        let Some(allocator) = self.routes.get_mut(shard.index()) else {
            debug_assert!(false, "track allocation targeted an unknown shard");
            return Err(RouteError::Exhausted { max_slots: 0 });
        };
        let (slot, epoch) = allocator.allocate(now)?;
        let key = self.keys.insert(identity);
        Ok(TrackAllocation {
            key,
            route: RouteHandle::new(RouteId::new(shard, slot), epoch),
        })
    }

    pub(crate) fn release(&mut self, allocation: TrackAllocation, now: Instant) {
        let removed = self.keys.remove(allocation.key);
        debug_assert!(
            removed.is_some(),
            "track allocation must be live at release"
        );
        let Some(allocator) = self.routes.get_mut(allocation.route.shard().index()) else {
            debug_assert!(false, "track release targeted an unknown shard");
            return;
        };
        allocator.retire(allocation.route.route.slot(), now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::ExternalRoomId;

    fn room() -> RoomId {
        RoomId::from_external(&ExternalRoomId::new("topology").unwrap())
    }

    fn participant(seed: u8) -> ParticipantId {
        ParticipantId::from_bytes([seed; 16])
    }

    fn identity(kind: TrackKind, publisher: u8) -> TrackIdentity {
        let publisher = participant(publisher);
        TrackIdentity {
            room_id: room(),
            publisher,
            id: publisher.derive_track_id(kind, "same-label"),
        }
    }

    #[test]
    fn room_wildcards_are_namespaced_by_track_kind() {
        let mut topology = TrackTopology::default();
        let audio = identity(TrackKind::Audio, 1);
        let data = identity(TrackKind::Data, 2);
        let subscriber = participant(3);
        let _ = topology.subscribe(
            subscriber,
            TrackSelector::RoomKind {
                room_id: room(),
                kind: TrackKind::Audio,
            },
        );

        assert!(topology.matches(audio).count() == 1);
        assert!(topology.matches(data).next().is_none());
    }

    #[test]
    fn publisher_selectors_do_not_cross_publishers() {
        let mut topology = TrackTopology::default();
        let first = identity(TrackKind::Video, 1);
        let second = identity(TrackKind::Video, 2);
        let _ = topology.subscribe(
            participant(3),
            TrackSelector::PublisherKind {
                room_id: room(),
                publisher: first.publisher,
                kind: TrackKind::Video,
            },
        );

        assert_eq!(topology.matches(first).count(), 1);
        assert_eq!(topology.matches(second).count(), 0);
    }

    #[test]
    fn unsubscribe_removes_only_the_matching_subscription() {
        let mut topology = TrackTopology::default();
        let track = identity(TrackKind::Audio, 1);
        let subscriber = participant(3);
        let selector = TrackSelector::Exact(track);
        let _ = topology.subscribe(subscriber, selector);
        let _ = topology.subscribe(participant(4), selector);

        assert!(topology.unsubscribe_matching(subscriber, selector));
        assert_eq!(topology.matches(track).count(), 1);
        assert!(!topology.unsubscribe_matching(subscriber, selector));
    }

    #[test]
    fn a_retired_destination_gets_a_fresh_track_route() {
        let mut allocator = TrackAllocator::new(2);
        let track = identity(TrackKind::Audio, 1);
        let first = allocator
            .allocate(ShardId::new(1), track, Instant::now())
            .expect("first destination allocation");
        allocator.release(first, Instant::now());
        let replacement = allocator
            .allocate(ShardId::new(1), track, Instant::now())
            .expect("replacement destination allocation");

        assert_ne!(first.key, replacement.key);
        assert_ne!(first.route, replacement.route);
    }

    #[test]
    fn removing_a_participant_removes_its_subscriptions_and_tracks() {
        let mut topology = TrackTopology::default();
        let owner = participant(1);
        let track = identity(TrackKind::Data, 1);
        let sub = topology.subscribe(owner, TrackSelector::Exact(track));
        assert!(topology.subscriptions.contains_key(sub));
        assert!(
            topology
                .publish(Track::data(
                    crate::track::TrackMeta {
                        room_id: room(),
                        shard_id: ShardId::new(0),
                        id: track.id,
                        origin: owner,
                    },
                    None,
                ))
                .is_some()
        );

        topology.remove_participant(owner);

        assert!(!topology.subscriptions.contains_key(sub));
        assert!(!topology.contains(track));
    }
}
