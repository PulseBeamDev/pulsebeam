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

use indexmap::IndexMap;

use crate::entity::{ParticipantId, TrackId};
use crate::id::ShardId;
use crate::keys::{DownstreamSlotKey, ParticipantKey};
use crate::route::RouteHandle;

/// One destination shard's interest in one stream.
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
/// Keyed by `(shard, stream)` rather than nested, because every question asked
/// of it is about one shard's interest in one stream — never "who subscribes
/// to this across the node", which is what a nested shape would optimise for.
#[derive(Debug)]
pub(crate) struct Subscriptions<K, S> {
    interest: IndexMap<(ShardId, K), Interest<S>>,
}

impl<K, S> Default for Subscriptions<K, S> {
    fn default() -> Self {
        Self {
            interest: IndexMap::new(),
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
        let interest = self.interest.entry((shard, stream)).or_default();
        let was_empty = interest.subscribers.is_empty();
        interest.subscribers.insert(subscriber, payload);
        if was_empty && interest.route.is_none() {
            InterestChange::Install
        } else {
            InterestChange::None
        }
    }

    /// Record the route installed for a shard's interest.
    pub fn installed(&mut self, shard: ShardId, stream: K, route: RouteHandle) {
        let interest = self.interest.entry((shard, stream)).or_default();
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
        let key = (shard, stream.clone());
        let Some(interest) = self.interest.get_mut(&key) else {
            return InterestChange::None;
        };
        if interest.subscribers.shift_remove(subscriber).is_none() {
            return InterestChange::None;
        }
        if !interest.subscribers.is_empty() {
            return InterestChange::None;
        }
        match interest.route.take() {
            Some(route) => {
                self.interest.shift_remove(&key);
                InterestChange::Retire { route }
            }
            None => {
                self.interest.shift_remove(&key);
                InterestChange::None
            }
        }
    }

    pub fn plan_destinations(
        &self,
        stream: &K,
    ) -> Vec<(ShardId, Option<RouteHandle>, Vec<S>)> {
        self.interest
            .iter()
            .filter_map(|((shard, candidate), interest)| {
                if candidate != stream {
                    return None;
                }
                Some((
                    *shard,
                    interest.route,
                    interest.subscribers.values().copied().collect(),
                ))
            })
            .collect()
    }

    pub fn remove_stream(&mut self, stream: &K) -> Vec<Retired> {
        let mut retired = Vec::new();
        self.interest.retain(|(shard, candidate), interest| {
            if candidate != stream {
                return true;
            }
            if let Some(route) = interest.route.take() {
                retired.push(Retired {
                    destination: *shard,
                    route,
                });
            }
            false
        });
        retired
    }

    /// Drop a participant from every stream it subscribed to, returning the
    /// routes that lose their last consumer as a result.
    pub fn remove_participant(&mut self, subscriber: &ParticipantId) -> Vec<Retired> {
        let mut retired = Vec::new();
        self.interest.retain(|(shard, _stream), interest| {
            if interest.subscribers.shift_remove(subscriber).is_none() {
                return true;
            }
            if !interest.subscribers.is_empty() {
                return true;
            }
            if let Some(route) = interest.route.take() {
                retired.push(Retired {
                    destination: *shard,
                    route,
                });
            }
            false
        });
        retired
    }

    #[cfg(test)]
    pub fn route_for(&self, shard: ShardId, stream: &K) -> Option<RouteHandle> {
        self.interest
            .get(&(shard, stream.clone()))
            .and_then(|i| i.route)
    }

    #[cfg(test)]
    pub fn subscriber_count(&self, shard: ShardId, stream: &K) -> usize {
        self.interest
            .get(&(shard, stream.clone()))
            .map_or(0, |i| i.subscribers.len())
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
}
