//! What a participant declared it wants, and which publications satisfy it.
//!
//! Video, audio and data all ask the same question — subscribers declare
//! patterns, publishers publish concrete subjects, and something has to match
//! them and keep the match current as both sides change. Answering it three
//! different ways is what produced three spellings of one subscriber map and
//! a room scan that replans every track on every join.
//!
//! A subject is `(room, publisher, kind, name)`. `kind` is not stored here:
//! the delivery key differs per kind, so there is one table per kind and the
//! type parameter carries the difference. Wildcards are allowed in `publisher`
//! and `name`, which makes the set of patterns that can match a given subject
//! finite and tiny — `2^2 = 4` — so matching is four hash lookups rather than
//! the subject trie a broker with arbitrary-depth subjects needs.
//!
//! Wildcards are only sound where the delivery key does not depend on the
//! publication. Audio picks a slot per packet from a shared pool and data's
//! channel belongs to the topic subscription, so both may wildcard. A video
//! slot is allocated per track, so a group spanning several video publications
//! could not carry one delivery key for all of them — video patterns are
//! always fully concrete, which is the existing "client names its tracks" API
//! rather than a new restriction.
#![deny(clippy::arithmetic_side_effects)]
// Wired in the step that moves audio onto patterns; standalone until then.
#![allow(dead_code)]

use arrayvec::ArrayVec;
use indexmap::{IndexMap, IndexSet};

type Map<K, V> = IndexMap<K, V>;
type Set<K> = IndexSet<K>;

use crate::entity::{ParticipantId, RoomId};
use crate::id::ShardId;
use crate::keys::ParticipantKey;
pub(crate) use crate::view::GroupId;

/// A concrete publication: what a publisher actually announced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Subject<N> {
    pub room: RoomId,
    pub publisher: ParticipantId,
    pub name: N,
}

/// A declaration: what a subscriber asked for. `None` is a wildcard.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Pattern<N> {
    pub room: RoomId,
    pub publisher: Option<ParticipantId>,
    pub name: Option<N>,
}

impl<N: Clone + Eq> Pattern<N> {
    /// One named publication from one publisher. The only form video may take.
    pub fn exact(room: RoomId, publisher: ParticipantId, name: N) -> Self {
        Self {
            room,
            publisher: Some(publisher),
            name: Some(name),
        }
    }

    /// One name, whoever publishes it — a pinned audio track, or a data topic
    /// subscribed across all publishers.
    pub fn any_publisher(room: RoomId, name: N) -> Self {
        Self {
            room,
            publisher: None,
            name: Some(name),
        }
    }

    /// Everything one publisher sends of this kind.
    pub fn any_name(room: RoomId, publisher: ParticipantId) -> Self {
        Self {
            room,
            publisher: Some(publisher),
            name: None,
        }
    }

    /// Everything of this kind in the room — audio `auto: true`.
    pub fn all(room: RoomId) -> Self {
        Self {
            room,
            publisher: None,
            name: None,
        }
    }

    pub fn matches(&self, subject: &Subject<N>) -> bool {
        self.room == subject.room
            && self
                .publisher
                .as_ref()
                .is_none_or(|p| *p == subject.publisher)
            && self.name.as_ref().is_none_or(|n| *n == subject.name)
    }

    /// Whether every subject `other` matches is also matched by `self`.
    ///
    /// Used to keep a participant from holding two patterns that both match one
    /// publication, which would deliver to it twice.
    pub fn subsumes(&self, other: &Self) -> bool {
        self.room == other.room
            && field_subsumes(self.publisher.as_ref(), other.publisher.as_ref())
            && field_subsumes(self.name.as_ref(), other.name.as_ref())
    }
}

fn field_subsumes<T: Eq>(broad: Option<&T>, narrow: Option<&T>) -> bool {
    match (broad, narrow) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(a), Some(b)) => a == b,
    }
}

impl<N: Clone + Eq> Subject<N> {
    /// Every pattern that can match this subject. Exactly four, because
    /// wildcards are permitted in exactly two positions.
    pub fn candidates(&self) -> [Pattern<N>; 4] {
        [
            Pattern::exact(self.room.clone(), self.publisher, self.name.clone()),
            Pattern::any_publisher(self.room.clone(), self.name.clone()),
            Pattern::any_name(self.room.clone(), self.publisher),
            Pattern::all(self.room.clone()),
        ]
    }
}

/// One subscriber's presence in a group: where it lives and how to hand it a
/// packet once it gets there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Member<S> {
    pub shard: ShardId,
    pub key: ParticipantKey,
    pub delivery: S,
}

/// What a declaration did to route lifecycle.
///
/// Routes are per `(publication, destination shard)`, so what the caller has to
/// act on is not the membership change itself but whether a shard just gained
/// its first consumer of the group, or lost its last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Membership {
    /// Nothing to install: the shard already consumed this group.
    Joined,
    /// This shard now needs routes for every publication matching the pattern.
    FirstOnShard,
    /// The participant already held this declaration.
    Unchanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Departure {
    /// Others on this shard still consume the group.
    Left,
    /// Nothing on this shard consumes it any more; its routes can retire.
    LastOnShard,
    /// The participant did not hold this declaration.
    Absent,
}

#[derive(Debug)]
struct Group<S> {
    members: Map<ParticipantId, Member<S>>,
    per_shard: Map<ShardId, u32>,
}

impl<S> Default for Group<S> {
    fn default() -> Self {
        Self {
            members: Map::default(),
            per_shard: Map::default(),
        }
    }
}

/// Declarations for one kind, and the groups they resolve to.
#[derive(Debug)]
pub(crate) struct PatternTable<N, S> {
    ids: Map<Pattern<N>, GroupId>,
    groups: Vec<Option<Group<S>>>,
    free: Vec<GroupId>,
    by_participant: Map<ParticipantId, Set<Pattern<N>>>,
}

impl<N, S> Default for PatternTable<N, S> {
    fn default() -> Self {
        Self {
            ids: Map::default(),
            groups: Vec::new(),
            free: Vec::new(),
            by_participant: Map::default(),
        }
    }
}

impl<N: std::hash::Hash + Eq + Clone, S: Copy> PatternTable<N, S> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `participant` wants everything matching `pattern`.
    ///
    /// Rejects a pattern subsumed by one the participant already holds, and
    /// drops any it subsumes, so no participant can match one publication
    /// twice. Returns what the caller owes route lifecycle, plus any groups
    /// the normalization emptied.
    pub fn declare(
        &mut self,
        pattern: Pattern<N>,
        participant: ParticipantId,
        member: Member<S>,
    ) -> (Membership, Vec<(GroupId, Departure)>) {
        let held = self.by_participant.entry(participant).or_default();
        if held.contains(&pattern) {
            return (Membership::Unchanged, Vec::new());
        }
        if held.iter().any(|existing| existing.subsumes(&pattern)) {
            return (Membership::Unchanged, Vec::new());
        }
        let narrowed: Vec<Pattern<N>> = held
            .iter()
            .filter(|existing| pattern.subsumes(existing))
            .cloned()
            .collect();

        let mut displaced = Vec::new();
        for stale in narrowed {
            if let Some(outcome) = self.retract(&stale, &participant) {
                displaced.push(outcome);
            }
        }

        self.by_participant
            .entry(participant)
            .or_default()
            .insert(pattern.clone());

        let id = self.intern(pattern);
        let Some(group) = self.groups.get_mut(id.0 as usize).and_then(Option::as_mut) else {
            debug_assert!(false, "an interned group id must resolve");
            return (Membership::Unchanged, displaced);
        };
        let shard = member.shard;
        group.members.insert(participant, member);
        let count = group.per_shard.entry(shard).or_insert(0);
        *count = count.saturating_add(1);
        let membership = if *count == 1 {
            Membership::FirstOnShard
        } else {
            Membership::Joined
        };
        (membership, displaced)
    }

    /// Drop one declaration.
    pub fn undeclare(&mut self, pattern: &Pattern<N>, participant: &ParticipantId) -> Departure {
        match self.retract(pattern, participant) {
            Some((_, departure)) => departure,
            None => Departure::Absent,
        }
    }

    /// Drop every declaration a participant held, for when it leaves.
    pub fn remove_participant(&mut self, participant: &ParticipantId) -> Vec<(GroupId, Departure)> {
        let Some(held) = self.by_participant.shift_remove(participant) else {
            return Vec::new();
        };
        held.into_iter()
            .filter_map(|pattern| self.retract_group(&pattern, participant))
            .collect()
    }

    fn retract(
        &mut self,
        pattern: &Pattern<N>,
        participant: &ParticipantId,
    ) -> Option<(GroupId, Departure)> {
        let held = self.by_participant.get_mut(participant)?;
        if !held.shift_remove(pattern) {
            return None;
        }
        if held.is_empty() {
            self.by_participant.shift_remove(participant);
        }
        self.retract_group(pattern, participant)
    }

    fn retract_group(
        &mut self,
        pattern: &Pattern<N>,
        participant: &ParticipantId,
    ) -> Option<(GroupId, Departure)> {
        let id = *self.ids.get(pattern)?;
        let group = self
            .groups
            .get_mut(id.0 as usize)
            .and_then(Option::as_mut)?;
        let member = group.members.shift_remove(participant)?;
        let departure = match group.per_shard.get_mut(&member.shard) {
            Some(count) => {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    group.per_shard.shift_remove(&member.shard);
                    Departure::LastOnShard
                } else {
                    Departure::Left
                }
            }
            None => {
                debug_assert!(false, "a member's shard must be counted");
                Departure::Left
            }
        };
        if group.members.is_empty() {
            self.ids.shift_remove(pattern);
            if let Some(slot) = self.groups.get_mut(id.0 as usize) {
                *slot = None;
            }
            self.free.push(id);
        }
        Some((id, departure))
    }

    fn intern(&mut self, pattern: Pattern<N>) -> GroupId {
        if let Some(id) = self.ids.get(&pattern) {
            return *id;
        }
        let id = match self.free.pop() {
            Some(id) => id,
            None => {
                let next = u32::try_from(self.groups.len()).unwrap_or(u32::MAX);
                self.groups.push(None);
                GroupId(next)
            }
        };
        if let Some(slot) = self.groups.get_mut(id.0 as usize) {
            *slot = Some(Group::default());
        }
        self.ids.insert(pattern, id);
        id
    }

    /// The groups a publication must be forwarded to. At most four, because
    /// only four patterns can match a subject.
    pub fn match_subject(&self, subject: &Subject<N>) -> ArrayVec<GroupId, 4> {
        let mut matched = ArrayVec::new();
        for candidate in subject.candidates() {
            if let Some(id) = self.ids.get(&candidate) {
                matched.push(*id);
            }
        }
        matched
    }

    /// Where a subscriber sits in a pattern's group, for addressing the op
    /// that removes it.
    pub fn member_key(
        &self,
        pattern: &Pattern<N>,
        participant: &ParticipantId,
    ) -> Option<(ShardId, ParticipantKey)> {
        let id = *self.ids.get(pattern)?;
        let group = self.groups.get(id.0 as usize).and_then(Option::as_ref)?;
        let member = group.members.get(participant)?;
        Some((member.shard, member.key))
    }

    /// Drop a pattern outright, returning its group and everyone who was in
    /// it. For a publication going away: its subscribers never unsubscribe, so
    /// without this their declarations would outlive the thing they named.
    pub fn retire_pattern(
        &mut self,
        pattern: &Pattern<N>,
    ) -> Option<(GroupId, Vec<(ParticipantId, ShardId, ParticipantKey)>)> {
        let id = self.ids.shift_remove(pattern)?;
        let group = self.groups.get_mut(id.0 as usize).and_then(Option::take)?;
        let members = group
            .members
            .iter()
            .map(|(participant, member)| (*participant, member.shard, member.key))
            .collect();
        for participant in group.members.keys() {
            if let Some(held) = self.by_participant.get_mut(participant) {
                held.shift_remove(pattern);
                if held.is_empty() {
                    self.by_participant.shift_remove(participant);
                }
            }
        }
        self.free.push(id);
        Some((id, members))
    }

    pub fn group_of(&self, pattern: &Pattern<N>) -> Option<GroupId> {
        self.ids.get(pattern).copied()
    }

    /// Members of a group that live on one shard — what that shard's compiled
    /// group image holds.
    ///
    /// Yields the participant id alongside the key because `ParticipantKey` is
    /// a per-shard arena key: two participants on different shards can hold the
    /// same one, so anything comparing identities across shards has to use the
    /// id.
    pub fn members_on(
        &self,
        group: GroupId,
        shard: ShardId,
    ) -> Vec<(ParticipantId, ParticipantKey, S)> {
        self.groups
            .get(group.0 as usize)
            .and_then(Option::as_ref)
            .map(|g| {
                g.members
                    .iter()
                    .filter(|(_, m)| m.shard == shard)
                    .map(|(id, m)| (*id, m.key, m.delivery))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Shards holding at least one member of the group.
    pub fn shards_of(&self, group: GroupId) -> Vec<ShardId> {
        self.groups
            .get(group.0 as usize)
            .and_then(Option::as_ref)
            .map(|g| g.per_shard.keys().copied().collect())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub fn member_count(&self, group: GroupId) -> usize {
        self.groups
            .get(group.0 as usize)
            .and_then(Option::as_ref)
            .map_or(0, |g| g.members.len())
    }

    #[cfg(test)]
    pub fn live_groups(&self) -> usize {
        self.ids.len()
    }

    /// Everything a participant declared, for a departure that has to reconcile
    /// the streams it was consuming before its declarations go.
    pub fn declarations_of(&self, participant: &ParticipantId) -> Vec<Pattern<N>> {
        self.by_participant
            .get(participant)
            .map(|held| held.iter().cloned().collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    // Convenience only: a test is not a shard, so nothing here is
    // cross-core. See docs/thread-per-core.md.
    use super::*;
    use crate::entity::ExternalRoomId;

    type Table = PatternTable<String, u8>;

    fn room(name: &str) -> RoomId {
        RoomId::from_external(&ExternalRoomId::new(name).unwrap())
    }

    fn pid(seed: u8) -> ParticipantId {
        ParticipantId::from_bytes([seed; 16])
    }

    fn member(shard: usize) -> Member<u8> {
        Member {
            shard: ShardId::new(shard),
            key: ParticipantKey::default(),
            delivery: 0,
        }
    }

    fn subject(r: RoomId, publisher: u8, name: &str) -> Subject<String> {
        Subject {
            room: r,
            publisher: pid(publisher),
            name: name.to_string(),
        }
    }

    /// The whole point of the fixed-arity subject space: a publication is
    /// matched by exactly the four patterns that can name it, and by nothing
    /// else. Every declaration form is exercised against one subject.
    #[test]
    fn a_subject_is_matched_by_every_form_that_names_it() {
        let mut table = Table::new();
        let r = room("r");
        let subj = subject(r.clone(), 1, "t");

        let forms = [
            Pattern::exact(r.clone(), pid(1), "t".to_string()),
            Pattern::any_publisher(r.clone(), "t".to_string()),
            Pattern::any_name(r.clone(), pid(1)),
            Pattern::all(r.clone()),
        ];
        // A distinct participant per form, so normalization does not
        // collapse them into one another.
        for (seed, form) in (10u8..).zip(forms.iter()) {
            assert!(form.matches(&subj), "{form:?} must match the subject");
            table.declare(form.clone(), pid(seed), member(0));
        }

        assert_eq!(
            table.match_subject(&subj).len(),
            4,
            "all four forms resolve to groups the publication must reach"
        );
    }

    /// Nothing that names a different room, publisher or track may match.
    #[test]
    fn a_subject_is_matched_by_nothing_that_names_something_else() {
        let r = room("r");
        let subj = subject(r.clone(), 1, "t");

        let misses = [
            Pattern::all(room("other")),
            Pattern::exact(r.clone(), pid(2), "t".to_string()),
            Pattern::exact(r.clone(), pid(1), "u".to_string()),
            Pattern::any_publisher(r.clone(), "u".to_string()),
            Pattern::any_name(r.clone(), pid(2)),
        ];
        for (i, miss) in misses.iter().enumerate() {
            assert!(!miss.matches(&subj), "pattern {i} must not match");
        }
    }

    /// Room isolation is structural here, not a convention: `room` is the one
    /// field with no wildcard, so no declaration can reach across rooms.
    #[test]
    fn no_pattern_can_span_rooms() {
        let mut table = Table::new();
        let broad = Pattern::all(room("a"));
        table.declare(broad, pid(1), member(0));

        assert!(
            table.match_subject(&subject(room("b"), 9, "t")).is_empty(),
            "the widest possible declaration still stops at its room"
        );
    }

    /// A participant holding both a wildcard and something the wildcard covers
    /// would receive one publication twice. The narrower declaration is
    /// dropped when the broader one arrives.
    #[test]
    fn a_broader_declaration_displaces_what_it_covers() {
        let mut table = Table::new();
        let r = room("r");
        let pin = Pattern::any_publisher(r.clone(), "t".to_string());
        let auto = Pattern::all(r.clone());

        table.declare(pin, pid(1), member(0));
        assert_eq!(table.live_groups(), 1);

        let (membership, displaced) = table.declare(auto.clone(), pid(1), member(0));
        assert_eq!(membership, Membership::FirstOnShard);
        assert_eq!(displaced.len(), 1, "the pin it covers is retracted");
        assert_eq!(
            table.declarations_of(&pid(1)),
            vec![auto],
            "only the broader declaration survives"
        );
        assert_eq!(
            table.match_subject(&subject(r, 1, "t")).len(),
            1,
            "the publication reaches this participant through one group only"
        );
    }

    /// The other order: a narrower declaration arriving under a live wildcard
    /// is redundant and must not create a second path to the same participant.
    #[test]
    fn a_narrower_declaration_under_a_wildcard_is_ignored() {
        let mut table = Table::new();
        let r = room("r");
        table.declare(Pattern::all(r.clone()), pid(1), member(0));

        let (membership, displaced) = table.declare(
            Pattern::any_publisher(r.clone(), "t".to_string()),
            pid(1),
            member(0),
        );
        assert_eq!(membership, Membership::Unchanged);
        assert!(displaced.is_empty());
        assert_eq!(table.live_groups(), 1, "no second group is created");
        assert_eq!(table.match_subject(&subject(r, 1, "t")).len(), 1);
    }

    /// Subsumption is what the overlap rule rests on, so check the lattice
    /// directly rather than only through declare().
    #[test]
    fn subsumption_orders_the_four_forms() {
        let r = room("r");
        let exact = Pattern::exact(r.clone(), pid(1), "t".to_string());
        let by_name = Pattern::any_publisher(r.clone(), "t".to_string());
        let by_pub = Pattern::any_name(r.clone(), pid(1));
        let all = Pattern::all(r.clone());

        for narrow in [&exact, &by_name, &by_pub, &all] {
            assert!(all.subsumes(narrow), "the room wildcard covers everything");
        }
        assert!(by_name.subsumes(&exact));
        assert!(by_pub.subsumes(&exact));
        assert!(!exact.subsumes(&by_name));
        assert!(!by_name.subsumes(&by_pub), "neither covers the other");
        assert!(!by_pub.subsumes(&by_name));
    }

    /// Routes are per (publication, shard), so what the caller acts on is a
    /// shard crossing zero, not every membership change.
    #[test]
    fn only_the_first_and_last_member_on_a_shard_move_routes() {
        let mut table = Table::new();
        let r = room("r");
        let auto = Pattern::all(r);

        let (first, _) = table.declare(auto.clone(), pid(1), member(0));
        assert_eq!(first, Membership::FirstOnShard);

        let (second, _) = table.declare(auto.clone(), pid(2), member(0));
        assert_eq!(second, Membership::Joined, "the shard already had routes");

        let (elsewhere, _) = table.declare(auto.clone(), pid(3), member(1));
        assert_eq!(
            elsewhere,
            Membership::FirstOnShard,
            "a second shard needs its own routes"
        );

        assert_eq!(table.undeclare(&auto, &pid(1)), Departure::Left);
        assert_eq!(table.undeclare(&auto, &pid(2)), Departure::LastOnShard);
        assert_eq!(
            table.undeclare(&auto, &pid(2)),
            Departure::Absent,
            "leaving twice is harmless"
        );
    }

    /// The 501st listener must be one membership write against a group that
    /// already exists — the property the whole model rests on.
    #[test]
    fn many_listeners_on_one_declaration_share_one_group() {
        let mut table = Table::new();
        let r = room("r");
        let auto = Pattern::all(r.clone());

        for seed in 0..64u8 {
            table.declare(auto.clone(), pid(seed), member(0));
        }
        assert_eq!(table.live_groups(), 1);

        let id = table.group_of(&auto).unwrap();
        assert_eq!(table.member_count(id), 64);
        assert_eq!(
            table.match_subject(&subject(r, 200, "t")),
            [id].into_iter().collect::<ArrayVec<GroupId, 4>>(),
            "any publication in the room resolves to that one group"
        );
    }

    /// A shard's compiled image holds only the members living on it.
    #[test]
    fn members_are_reported_per_shard() {
        let mut table = Table::new();
        let auto = Pattern::all(room("r"));
        table.declare(auto.clone(), pid(1), member(0));
        table.declare(auto.clone(), pid(2), member(0));
        table.declare(auto.clone(), pid(3), member(1));

        let id = table.group_of(&auto).unwrap();
        assert_eq!(table.members_on(id, ShardId::new(0)).len(), 2);
        assert_eq!(table.members_on(id, ShardId::new(1)).len(), 1);
        assert_eq!(table.members_on(id, ShardId::new(2)).len(), 0);
        assert_eq!(table.shards_of(id).len(), 2);
    }

    /// A departure takes every declaration with it, and empties the groups it
    /// was alone in.
    #[test]
    fn a_departure_retracts_everything_it_declared() {
        let mut table = Table::new();
        let r = room("r");
        let solo = Pattern::any_publisher(r.clone(), "solo".to_string());
        let shared = Pattern::any_publisher(r.clone(), "shared".to_string());

        table.declare(solo.clone(), pid(1), member(0));
        table.declare(shared.clone(), pid(1), member(0));
        table.declare(shared.clone(), pid(2), member(0));
        assert_eq!(table.live_groups(), 2);

        let outcomes = table.remove_participant(&pid(1));
        assert_eq!(outcomes.len(), 2, "both declarations are retracted");
        assert_eq!(table.live_groups(), 1, "the group it was alone in dies");
        assert!(table.group_of(&solo).is_none());
        assert_eq!(table.member_count(table.group_of(&shared).unwrap()), 1);
        assert!(table.declarations_of(&pid(1)).is_empty());
    }

    /// Subscribing before anything is published is not a special case: the
    /// declaration simply matches nothing yet, and matches the publication the
    /// moment it appears. The parked-subscriber map this replaces existed only
    /// to express that.
    #[test]
    fn a_declaration_made_before_the_publication_matches_it_on_arrival() {
        let mut table = Table::new();
        let r = room("r");
        let wanted = Pattern::any_publisher(r.clone(), "t".to_string());
        table.declare(wanted, pid(1), member(0));

        let arriving = subject(r, 9, "t");
        assert_eq!(
            table.match_subject(&arriving).len(),
            1,
            "a publication appearing later resolves to the waiting declaration"
        );
    }

    /// And withdrawing it stops publications that appear afterwards from
    /// reaching the subscriber - the failure here is silent, since nothing
    /// errors, the subscriber just keeps receiving.
    #[test]
    fn a_withdrawn_declaration_does_not_reach_later_publications() {
        let mut table = Table::new();
        let r = room("r");
        let wanted = Pattern::any_publisher(r.clone(), "t".to_string());

        table.declare(wanted.clone(), pid(1), member(0));
        assert_eq!(table.undeclare(&wanted, &pid(1)), Departure::LastOnShard);

        assert!(
            table.match_subject(&subject(r, 9, "t")).is_empty(),
            "a publication appearing after the withdrawal reaches nobody"
        );
    }

    /// A group id is recycled once its group dies. The shard resolves groups
    /// by bare array index, so ids must stay dense rather than grow forever.
    #[test]
    fn group_ids_are_dense_and_recycled() {
        let mut table = Table::new();
        let r = room("r");
        let first = Pattern::any_publisher(r.clone(), "a".to_string());

        table.declare(first.clone(), pid(1), member(0));
        let original = table.group_of(&first).unwrap();
        table.remove_participant(&pid(1));
        assert!(table.group_of(&first).is_none());

        let second = Pattern::any_publisher(r, "b".to_string());
        table.declare(second.clone(), pid(2), member(0));
        assert_eq!(
            table.group_of(&second).unwrap(),
            original,
            "the dead group's index is reused rather than left as a hole"
        );
    }

    /// Declaring the same thing twice must not double-count the shard, or the
    /// last member leaving would never retire the routes.
    #[test]
    fn a_repeated_declaration_does_not_inflate_the_shard_count() {
        let mut table = Table::new();
        let auto = Pattern::all(room("r"));

        assert_eq!(
            table.declare(auto.clone(), pid(1), member(0)).0,
            Membership::FirstOnShard
        );
        assert_eq!(
            table.declare(auto.clone(), pid(1), member(0)).0,
            Membership::Unchanged
        );
        assert_eq!(table.member_count(table.group_of(&auto).unwrap()), 1);
        assert_eq!(table.undeclare(&auto, &pid(1)), Departure::LastOnShard);
    }
}
