//! Who receives what, and therefore which routes exist.
//!
//! This is the decision the shard used to make. It counted its own local
//! subscribers, concluded it needed a cluster route, and — unable to mint one
//! — had to ask for it, which is where a request id, a pending map and a
//! completion handler came from. All of that was scaffolding around a
//! decision sitting on the wrong side.
//!
//! The control plane already sees every subscribe and unsubscribe, already
//! knows which shard each participant is on, and is the only thing that may
//! allocate a route. So it decides: a destination shard's first subscriber
//! for a stream is what creates the route, the last one leaving is what
//! retires it, and the shard is told by its published view rather than by an
//! answer to a question.
#![deny(clippy::arithmetic_side_effects)]

use indexmap::{IndexMap, IndexSet};

use crate::entity::{ParticipantId, TrackId};
use crate::id::ShardId;
use crate::keys::{DownstreamSlotKey, ParticipantKey};
use crate::route::RouteHandle;

/// One destination shard's interest in one stream: who wants it there, and
/// the route serving them.
///
/// The destination-local forwarding key is deliberately *not* here. It
/// outlives the interest — video keeps its fanout key across an unsubscribe so
/// a resubscribe does not mint a second one and abandon the first in the
/// arena — so tying it to a record that dies with the last subscriber would
/// leak arena slots.
#[derive(Debug)]
struct Interest<S> {
    subscribers: IndexMap<ParticipantId, S>,
    route: Option<RouteHandle>,
}

impl<S> Default for Interest<S> {
    fn default() -> Self {
        Self {
            subscribers: IndexMap::new(),
            route: None,
        }
    }
}

/// What the caller must do about a change in interest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InterestChange {
    /// Nothing to do: the shard already had, or still has, other subscribers.
    None,
    /// This shard now needs a route for the stream.
    Install,
    /// Nothing on this shard consumes the stream any more.
    Retire { route: RouteHandle },
}

/// Per-stream subscriber sets, keyed by the destination shard that holds
/// them.
///
/// Nested stream-then-shard rather than flat `(shard, stream)`. A flat key
/// makes `subscribe` a single lookup, but every *per-stream* question —
/// `plan_destinations` on the publish path, `remove_stream` on teardown — then
/// has to scan every interest on the node to find the handful belonging to one
/// stream. Nesting keeps both O(shards holding that stream), and costs the
/// point lookups only a second hash.
///
/// `by_participant` exists for the same reason on the other axis: a departure
/// should cost what that participant subscribed to, not what the node holds.
///
/// Removal is `shift_remove` throughout, not `swap_remove`. Iteration order
/// here becomes the order of `local_subscribers` and `remote_routes` in a
/// published plan, so reordering on removal would change packet ordering under
/// simulation — deterministic still, but differently, which would make any
/// seed regression ambiguous. The maps that pay for it are small: subscribers
/// on one shard for one stream, and shards holding one stream.
#[derive(Debug)]
pub(crate) struct Subscriptions<K, S> {
    interest: IndexMap<K, IndexMap<ShardId, Interest<S>>>,
    by_participant: IndexMap<ParticipantId, IndexSet<(K, ShardId)>>,
    #[cfg(test)]
    visited: std::cell::Cell<usize>,
}

impl<K, S> Default for Subscriptions<K, S> {
    fn default() -> Self {
        Self {
            interest: IndexMap::new(),
            by_participant: IndexMap::new(),
            #[cfg(test)]
            visited: std::cell::Cell::new(0),
        }
    }
}

impl<K: std::hash::Hash + Eq + Clone, S: Copy> Subscriptions<K, S> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns [`InterestChange::Install`] only for the first subscriber on
    /// that shard.
    pub fn subscribe(
        &mut self,
        shard: ShardId,
        stream: K,
        subscriber: ParticipantId,
        payload: S,
        _publisher_shard: ShardId,
    ) -> InterestChange {
        let interest = self
            .interest
            .entry(stream.clone())
            .or_default()
            .entry(shard)
            .or_default();
        let was_empty = interest.subscribers.is_empty();
        interest.subscribers.insert(subscriber, payload);
        let needs_route = was_empty && interest.route.is_none();

        self.by_participant
            .entry(subscriber)
            .or_default()
            .insert((stream, shard));

        if needs_route {
            InterestChange::Install
        } else {
            InterestChange::None
        }
    }

    /// Record the route installed for a shard's interest.
    pub fn installed(&mut self, shard: ShardId, stream: K, route: RouteHandle) {
        let interest = self
            .interest
            .entry(stream)
            .or_default()
            .entry(shard)
            .or_default();
        debug_assert!(
            interest.route.is_none(),
            "a shard's interest holds one route at a time"
        );
        interest.route = Some(route);
    }

    /// Returns [`InterestChange::Retire`] only when the last subscriber on
    /// that shard leaves.
    pub fn unsubscribe(
        &mut self,
        shard: ShardId,
        stream: &K,
        subscriber: &ParticipantId,
    ) -> InterestChange {
        let Some(shards) = self.interest.get_mut(stream) else {
            return InterestChange::None;
        };
        let Some(interest) = shards.get_mut(&shard) else {
            return InterestChange::None;
        };
        if interest.subscribers.shift_remove(subscriber).is_none() {
            return InterestChange::None;
        }
        Self::forget_subscription(&mut self.by_participant, subscriber, stream, shard);
        if !interest.subscribers.is_empty() {
            return InterestChange::None;
        }
        let route = interest.route.take();
        shards.shift_remove(&shard);
        if shards.is_empty() {
            self.interest.shift_remove(stream);
        }
        match route {
            Some(route) => InterestChange::Retire { route },
            None => InterestChange::None,
        }
    }

    fn forget_subscription(
        by_participant: &mut IndexMap<ParticipantId, IndexSet<(K, ShardId)>>,
        subscriber: &ParticipantId,
        stream: &K,
        shard: ShardId,
    ) {
        let Some(owned) = by_participant.get_mut(subscriber) else {
            return;
        };
        owned.shift_remove(&(stream.clone(), shard));
        if owned.is_empty() {
            by_participant.shift_remove(subscriber);
        }
    }

    pub fn plan_destinations(&self, stream: &K) -> Vec<(ShardId, Option<RouteHandle>, Vec<S>)> {
        let Some(shards) = self.interest.get(stream) else {
            return Vec::new();
        };
        shards
            .iter()
            .map(|(shard, interest)| {
                #[cfg(test)]
                self.visited.set(self.visited.get().saturating_add(1));
                (
                    *shard,
                    interest.route,
                    interest.subscribers.values().copied().collect(),
                )
            })
            .collect()
    }

    pub fn remove_stream(&mut self, stream: &K) -> Vec<Retired> {
        let Some(shards) = self.interest.shift_remove(stream) else {
            return Vec::new();
        };
        let mut retired = Vec::new();
        for (shard, mut interest) in shards {
            for subscriber in interest.subscribers.keys() {
                Self::forget_subscription(&mut self.by_participant, subscriber, stream, shard);
            }
            if let Some(route) = interest.route.take() {
                retired.push(Retired {
                    destination: shard,
                    route,
                });
            }
        }
        retired
    }

    /// Drop a participant from every stream it subscribed to, returning the
    /// routes that lose their last consumer as a result.
    pub fn remove_participant(&mut self, subscriber: &ParticipantId) -> Vec<Retired> {
        let Some(owned) = self.by_participant.shift_remove(subscriber) else {
            return Vec::new();
        };
        let mut retired = Vec::new();
        for (stream, shard) in owned {
            let Some(shards) = self.interest.get_mut(&stream) else {
                continue;
            };
            let Some(interest) = shards.get_mut(&shard) else {
                continue;
            };
            if interest.subscribers.shift_remove(subscriber).is_none() {
                continue;
            }
            if !interest.subscribers.is_empty() {
                continue;
            }
            if let Some(route) = interest.route.take() {
                retired.push(Retired {
                    destination: shard,
                    route,
                });
            }
            shards.shift_remove(&shard);
            if shards.is_empty() {
                self.interest.shift_remove(&stream);
            }
        }
        retired
    }

    #[cfg(test)]
    pub fn route_for(&self, shard: ShardId, stream: &K) -> Option<RouteHandle> {
        self.interest
            .get(stream)
            .and_then(|shards| shards.get(&shard))
            .and_then(|i| i.route)
    }

    #[cfg(test)]
    pub fn subscriber_count(&self, shard: ShardId, stream: &K) -> usize {
        self.interest
            .get(stream)
            .and_then(|shards| shards.get(&shard))
            .map_or(0, |i| i.subscribers.len())
    }

    /// How many interest entries `plan_destinations` has walked since the last
    /// reset. Structural: it must not grow with streams the caller did not ask
    /// about.
    #[cfg(test)]
    pub fn take_visited(&self) -> usize {
        self.visited.replace(0)
    }
}

/// A route that lost its last consumer.
#[derive(Debug, Clone)]
pub(crate) struct Retired {
    pub destination: ShardId,
    pub route: RouteHandle,
}

/// The video/audio flavour, keyed by track.
pub(crate) type TrackSubscriber = (ParticipantKey, DownstreamSlotKey);
pub(crate) type TrackSubscriptions = Subscriptions<TrackId, TrackSubscriber>;

#[cfg(test)]
mod tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;
    use crate::route::RouteId;

    fn pid(seed: u8) -> ParticipantId {
        ParticipantId::from_bytes([seed; 16])
    }

    fn track(seed: u8) -> TrackId {
        pid(seed).derive_track_id(crate::entity::TrackKind::Video, "v")
    }

    fn handle(slot: u32) -> RouteHandle {
        RouteHandle::new(RouteId::new(ShardId::new(0), slot), 0)
    }

    #[test]
    fn only_the_first_subscriber_on_a_shard_needs_a_route() {
        let mut subs = TrackSubscriptions::new();
        let shard = ShardId::new(1);
        let t = track(9);

        assert_eq!(
            subs.subscribe(
                shard,
                t,
                pid(1),
                (ParticipantKey::default(), DownstreamSlotKey::default()),
                ShardId::new(9)
            ),
            InterestChange::Install,
            "the first subscriber creates the route"
        );
        subs.installed(shard, t, handle(0));
        assert_eq!(
            subs.subscribe(
                shard,
                t,
                pid(2),
                (ParticipantKey::default(), DownstreamSlotKey::default()),
                ShardId::new(9)
            ),
            InterestChange::None,
            "a second subscriber joins the route that exists"
        );
    }

    #[test]
    fn only_the_last_subscriber_leaving_retires_the_route() {
        let mut subs = TrackSubscriptions::new();
        let shard = ShardId::new(1);
        let t = track(9);

        subs.subscribe(
            shard,
            t,
            pid(1),
            (ParticipantKey::default(), DownstreamSlotKey::default()),
            ShardId::new(9),
        );
        subs.installed(shard, t, handle(0));
        subs.subscribe(
            shard,
            t,
            pid(2),
            (ParticipantKey::default(), DownstreamSlotKey::default()),
            ShardId::new(9),
        );

        assert_eq!(subs.unsubscribe(shard, &t, &pid(1)), InterestChange::None);
        assert_eq!(
            subs.unsubscribe(shard, &t, &pid(2)),
            InterestChange::Retire { route: handle(0) }
        );
    }

    /// Two shards subscribing to the same track are two routes. A count that
    /// ignored the shard would install one and starve the other.
    #[test]
    fn each_destination_shard_gets_its_own_route() {
        let mut subs = TrackSubscriptions::new();
        let t = track(9);

        assert_eq!(
            subs.subscribe(
                ShardId::new(1),
                t,
                pid(1),
                (ParticipantKey::default(), DownstreamSlotKey::default()),
                ShardId::new(9)
            ),
            InterestChange::Install
        );
        assert_eq!(
            subs.subscribe(
                ShardId::new(2),
                t,
                pid(2),
                (ParticipantKey::default(), DownstreamSlotKey::default()),
                ShardId::new(9)
            ),
            InterestChange::Install,
            "a second shard needs its own route"
        );
    }

    /// A repeated subscribe must not inflate anything, and an unsubscribe for
    /// somebody who never subscribed must not retire a live route.
    #[test]
    fn duplicate_and_unknown_membership_changes_are_harmless() {
        let mut subs = TrackSubscriptions::new();
        let shard = ShardId::new(0);
        let t = track(9);

        subs.subscribe(
            shard,
            t,
            pid(1),
            (ParticipantKey::default(), DownstreamSlotKey::default()),
            ShardId::new(9),
        );
        subs.installed(shard, t, handle(0));
        assert_eq!(
            subs.subscribe(
                shard,
                t,
                pid(1),
                (ParticipantKey::default(), DownstreamSlotKey::default()),
                ShardId::new(9)
            ),
            InterestChange::None
        );
        assert_eq!(subs.subscriber_count(shard, &t), 1);

        assert_eq!(
            subs.unsubscribe(shard, &t, &pid(7)),
            InterestChange::None,
            "a stranger leaving must not retire the route"
        );
        assert_eq!(
            subs.unsubscribe(shard, &t, &pid(1)),
            InterestChange::Retire { route: handle(0) }
        );
    }

    /// A participant going away takes its subscriptions with it, wherever
    /// they were.
    #[test]
    fn removing_a_participant_retires_what_only_it_consumed() {
        let mut subs = TrackSubscriptions::new();
        let (a, b) = (ShardId::new(0), ShardId::new(1));
        let (t1, t2) = (track(1), track(2));

        subs.subscribe(
            a,
            t1,
            pid(1),
            (ParticipantKey::default(), DownstreamSlotKey::default()),
            ShardId::new(9),
        );
        subs.installed(a, t1, handle(0));
        subs.subscribe(
            a,
            t2,
            pid(1),
            (ParticipantKey::default(), DownstreamSlotKey::default()),
            ShardId::new(9),
        );
        subs.installed(a, t2, handle(1));
        subs.subscribe(
            b,
            t1,
            pid(2),
            (ParticipantKey::default(), DownstreamSlotKey::default()),
            ShardId::new(9),
        );
        subs.installed(b, t1, handle(2));

        let retired = subs.remove_participant(&pid(1));
        assert_eq!(retired.len(), 2, "both of its routes lose their consumer");
        assert!(
            subs.route_for(b, &t1).is_some(),
            "another shard's route for the same track survives"
        );
    }

    /// The structural point of the nested shape: answering "where does this
    /// stream go" must not depend on how many other streams the node holds.
    /// Asserted as entries walked, not elapsed time, so it cannot go flaky.
    #[test]
    fn planning_one_stream_does_not_walk_the_others() {
        let mut subs = TrackSubscriptions::new();
        let shard = ShardId::new(1);
        let target = track(200);

        for seed in 0..64u8 {
            subs.subscribe(
                shard,
                track(seed),
                pid(seed),
                (ParticipantKey::default(), DownstreamSlotKey::default()),
                ShardId::new(9),
            );
        }
        subs.subscribe(
            shard,
            target,
            pid(1),
            (ParticipantKey::default(), DownstreamSlotKey::default()),
            ShardId::new(9),
        );
        subs.subscribe(
            ShardId::new(2),
            target,
            pid(2),
            (ParticipantKey::default(), DownstreamSlotKey::default()),
            ShardId::new(9),
        );

        let _ = subs.take_visited();
        let destinations = subs.plan_destinations(&target);
        assert_eq!(destinations.len(), 2, "both shards holding it are planned");
        assert_eq!(
            subs.take_visited(),
            2,
            "only the target stream's own interest entries are walked"
        );

        assert_eq!(subs.plan_destinations(&track(250)), Vec::new());
        assert_eq!(
            subs.take_visited(),
            0,
            "a stream nobody subscribes to walks nothing at all"
        );
    }

    /// A departure costs what that participant subscribed to. The routes it
    /// was the last consumer of retire; the ones it shared do not.
    #[test]
    fn removing_a_participant_leaves_shared_routes_alone() {
        let mut subs = TrackSubscriptions::new();
        let shard = ShardId::new(0);
        let (solo, shared) = (track(1), track(2));

        for (t, who) in [(solo, pid(1)), (shared, pid(1)), (shared, pid(2))] {
            subs.subscribe(
                shard,
                t,
                who,
                (ParticipantKey::default(), DownstreamSlotKey::default()),
                ShardId::new(9),
            );
        }
        subs.installed(shard, solo, handle(0));
        subs.installed(shard, shared, handle(1));

        let retired = subs.remove_participant(&pid(1));
        assert_eq!(retired.len(), 1, "only the route it alone consumed retires");
        assert_eq!(retired[0].route, handle(0));
        assert_eq!(
            subs.subscriber_count(shard, &shared),
            1,
            "the co-subscriber keeps the shared route"
        );
        assert!(subs.route_for(shard, &shared).is_some());
    }

    /// Removing a stream must also forget it on the participant axis, or a
    /// later departure would try to retire a route that no longer exists.
    #[test]
    fn removing_a_stream_forgets_it_on_both_axes() {
        let mut subs = TrackSubscriptions::new();
        let shard = ShardId::new(0);
        let t = track(5);

        subs.subscribe(
            shard,
            t,
            pid(1),
            (ParticipantKey::default(), DownstreamSlotKey::default()),
            ShardId::new(9),
        );
        subs.installed(shard, t, handle(0));

        let retired = subs.remove_stream(&t);
        assert_eq!(retired.len(), 1);
        assert!(
            subs.remove_participant(&pid(1)).is_empty(),
            "the participant no longer holds a subscription to a dead stream"
        );
    }
}
