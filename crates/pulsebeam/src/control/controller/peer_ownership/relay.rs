//! Association-local relay labels advertised from acknowledged shard routes.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use pulsebeam_core::fabric::{MAJOR, Profile};
use pulsebeam_link::{BindingId, NetworkId, NodeRouteKey};
use pulsebeam_runtime::{sync::oneshot, time::Instant};
use serde::{Deserialize, Serialize};

use super::ControllerActor;
use crate::{
    entity::{ProjectId, ProjectRoomKey, PublicationId, PublicationKey, RoomId, TrackKind},
    id::ShardId,
    link::ReceiveRoute,
    route::{
        NodeRouteAddress,
        fabric::{DataPublisher, LiveRelayChange},
    },
    shard_update::PublisherAnchor,
};

pub(in crate::control) mod fabric;
pub(super) mod wan_publisher;
pub(super) mod wan_repair;
pub(in crate::control) mod wan_source;
pub(super) mod wan_transit;

pub(in crate::control) const LIMIT: usize = 128;

#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
pub(in crate::control) struct Key {
    room: ProjectRoomKey,
    publication: PublicationId,
    kind: TrackKind,
    reverse: bool,
}

impl Key {
    pub(in crate::control) fn new(
        room: ProjectRoomKey,
        publication: PublicationKey,
        reverse: bool,
    ) -> Self {
        Self {
            room,
            publication: publication.publication_id(),
            kind: publication.kind(),
            reverse,
        }
    }

    fn publication(self) -> PublicationKey {
        PublicationKey::new(self.kind, self.publication)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::control) struct Offer {
    project: ProjectId,
    room: RoomId,
    publication: PublicationId,
    kind: u8,
    reverse: bool,
    label: [u8; 4],
}

pub(in crate::control) type Offers = BTreeMap<Key, NodeRouteKey>;

pub(in crate::control) fn decode(offers: Box<[Offer]>) -> Option<Offers> {
    if offers.len() > LIMIT {
        return None;
    }
    let mut routes = BTreeMap::new();
    let mut labels = BTreeSet::new();
    for offer in offers {
        let kind = match offer.kind {
            0 => TrackKind::Audio,
            1 => TrackKind::Video,
            2 if !offer.reverse => TrackKind::Data,
            _ => return None,
        };
        let key = Key {
            room: ProjectRoomKey {
                project: offer.project,
                room: offer.room,
            },
            publication: offer.publication,
            kind,
            reverse: offer.reverse,
        };
        if offer.label == [0; 4]
            || !labels.insert(offer.label)
            || routes
                .insert(key, NodeRouteKey::from_bytes(offer.label))
                .is_some()
        {
            return None;
        }
    }
    Some(routes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_context_and_duplicated_route_labels_cannot_enter_media_offers() {
        let project = ProjectId::new();
        let room = RoomId::new("stage").unwrap();
        let publication = PublicationId::mint();
        let offer = |label, reverse| Offer {
            project,
            room,
            publication,
            kind: TrackKind::Video as u8,
            reverse,
            label,
        };
        let label = 1_u32.to_be_bytes();
        assert!(decode(Box::new([offer([0; 4], false)])).is_none());
        assert!(decode(Box::new([offer(label, false), offer(label, true)])).is_none());
        assert!(
            decode(Box::new([
                offer(label, false),
                offer(2_u32.to_be_bytes(), false)
            ]))
            .is_none()
        );
        let offered = decode(Box::new([offer(label, false)])).unwrap();
        assert_eq!(
            offered.get(&Key::new(
                ProjectRoomKey { project, room },
                PublicationKey::Video(publication),
                false
            )),
            Some(&NodeRouteKey::from_bytes(label))
        );
    }
}

struct Receive {
    destinations: Vec<NodeRouteAddress>,
    route: ReceiveRoute,
}

struct FabricReceive {
    publisher: PublisherAnchor,
    revision: u8,
    destinations: Vec<NodeRouteAddress>,
    staged: crate::control::link::StagedLiveReceive,
    acknowledged: bool,
}

impl FabricReceive {
    fn retained(&mut self, links: &crate::control::link::Links) -> bool {
        if !self.acknowledged {
            match self.staged.receipt.try_recv() {
                Ok(Ok(route)) => self.acknowledged = route.binding == self.staged.binding,
                Err(oneshot::error::TryRecvError::Empty) => return true,
                _ => return false,
            }
        }
        self.acknowledged && links.fabric_receive_live(self.staged.binding)
    }
}

struct FabricTarget {
    publisher: PublisherAnchor,
    revision: u8,
    destinations: Vec<NodeRouteAddress>,
    anchor: Option<u64>,
}

struct FabricSnapshot {
    deadline: Instant,
    routes: Box<[fabric::LiveRouteAck]>,
    anchors: super::ownership::wire::DataAnchors,
    opus: Option<(u8, super::ownership::wire::OpusAnchors)>,
}

#[derive(Default)]
pub(super) struct Relays {
    receives: BTreeMap<Key, Receive>,
    sends: BTreeMap<(Key, ShardId), (NodeRouteKey, BindingId)>,
    remote: Option<(Instant, Offers)>,
    fabric_remote: Option<FabricSnapshot>,
    fabric_pending: HashMap<BindingId, oneshot::Receiver<bool>>,
    publisher_pending: HashMap<BindingId, oneshot::Receiver<bool>>,
    fabric_receives: BTreeMap<Key, FabricReceive>,
}

pub(super) fn reconcile_wan_receives(
    actor: &mut ControllerActor,
    peer: NetworkId,
    session: super::Session,
    remote: Option<(Instant, &[wan_source::WanSourceWindow])>,
    now: Instant,
) {
    let Some(links) = &mut actor.links else {
        return;
    };
    let mut desired = Vec::new();
    let mut expires = now;
    if let Some((deadline, sources)) = remote.filter(|(deadline, _)| now < *deadline)
        && actor
            .receipt_trust
            .as_ref()
            .is_some_and(crate::control::controller::ownership::receipt::Authority::ready)
    {
        expires = deadline;
        let mut targets = HashMap::new();
        desired.extend(sources.iter().filter_map(|&source| {
            let publication = source.publisher.publication;
            let canonical = actor.lifecycle.ingress_peer(source.room, publication)?;
            if canonical != peer
                && (!links.is_wan_peer(canonical)
                    || !actor
                        .peer_ownership
                        .session_facts(session, now)
                        .any(|(key, fact)| {
                            source.matches_fact(*key, fact)
                                && actor.peer_ownership.network_fact(canonical, *key, now)
                                    == Some(fact)
                        }))
            {
                return None;
            }
            let track = actor.lifecycle.source_exact(source.room, publication)?;
            if track.meta().connection_id != source.publisher.connection
                || (publication.kind() == TrackKind::Data
                    && track.fabric_data_source() != Some(source.publisher.incarnation))
            {
                return None;
            }
            let routes = targets
                .entry(canonical)
                .or_insert_with(|| links.relay_targets(canonical, LIMIT));
            Some((
                source,
                canonical,
                routes.as_ref()?.get(&source.scope())?.clone(),
            ))
        }));
    }
    links.retain_wan_live_receives(peer, |ack, canonical, children| {
        if let Some(index) = desired.iter().position(|(source, selected, destinations)| {
            *selected == canonical
                && source.matches_ack(ack)
                && children
                    .iter()
                    .map(|child| child.destination)
                    .eq(destinations.iter().copied())
        }) {
            desired.swap_remove(index);
            true
        } else {
            false
        }
    });
    for (source, canonical_ingress, addresses) in desired {
        let _ = actor.stage_peer_wan_live_receive(
            now,
            crate::control::link::wan::WanLiveReceiveSpec {
                live: crate::control::link::LiveReceiveSpec {
                    peer,
                    room: source.room,
                    publication: source.publisher.publication,
                    incarnation: source.publisher.incarnation,
                    sequence_anchor: source.sequence_anchor,
                    major: MAJOR,
                    revision: pulsebeam_core::fabric::REGISTRY_REVISION,
                    emission: source.publisher.emission,
                    addresses,
                },
                canonical_ingress,
                start: source.window_start,
                forms: source.forms(),
                expires,
            },
        );
    }
}

impl Relays {
    pub(super) fn offers(&self) -> Box<[Offer]> {
        self.receives
            .iter()
            .map(|(key, receive)| Offer {
                project: key.room.project,
                room: key.room.room,
                publication: key.publication,
                kind: key.kind as u8,
                reverse: key.reverse,
                label: receive.route.address.to_bytes(),
            })
            .collect()
    }

    pub(super) fn install(
        &mut self,
        offers: Offers,
        fabric: Box<[fabric::LiveRouteAck]>,
        anchors: super::ownership::wire::DataAnchors,
        opus: Option<(u8, super::ownership::wire::OpusAnchors)>,
        deadline: Instant,
    ) {
        self.remote = Some((deadline, offers));
        self.fabric_remote = Some(FabricSnapshot {
            deadline,
            routes: fabric,
            anchors,
            opus,
        });
    }

    fn reconcile_fabric_receives(
        &mut self,
        actor: &mut ControllerActor,
        peer: NetworkId,
        now: Instant,
        authority: bool,
    ) {
        let Some(links) = &mut actor.links else {
            return;
        };
        let mut desired = BTreeMap::new();
        if let Some(snapshot) = self
            .fabric_remote
            .as_ref()
            .filter(|snapshot| authority && now < snapshot.deadline)
            && let Some(targets) = links.relay_targets(peer, LIMIT)
        {
            desired.extend(targets.into_iter().filter_map(|(key, destinations)| {
                let publication = key.publication();
                if key.reverse || actor.lifecycle.ingress_peer(key.room, publication) != Some(peer)
                {
                    return None;
                }
                let track = actor.lifecycle.source_exact(key.room, publication)?;
                let connection = track.meta().connection_id;
                let (publisher, revision, anchor) = match key.kind {
                    TrackKind::Data => {
                        let publisher = DataPublisher {
                            publication,
                            connection,
                            incarnation: track.fabric_data_source()?,
                        };
                        let anchor = snapshot
                            .anchors
                            .get(&publication)
                            .filter(|(sampled, _)| *sampled == publisher)
                            .map(|(_, sequence)| *sequence);
                        (publisher.into(), 0, anchor)
                    }
                    TrackKind::Audio if cfg!(feature = "sim") => {
                        let (revision, anchors) = snapshot.opus.as_ref()?;
                        let &(publisher, anchor) = anchors.get(&publication)?;
                        if publisher.publication != publication
                            || publisher.connection != connection
                            || publisher.emission.profile() != Profile::Opus
                            || !publisher.emission.fits_registry(MAJOR, *revision)
                        {
                            return None;
                        }
                        (publisher, *revision, Some(anchor))
                    }
                    _ => return None,
                };
                Some((
                    key,
                    FabricTarget {
                        publisher,
                        revision,
                        destinations,
                        anchor,
                    },
                ))
            }));
        }
        self.fabric_receives.retain(|key, receive| {
            let same = desired.get(key).is_some_and(|target| {
                target.publisher == receive.publisher
                    && target.revision == receive.revision
                    && target.destinations == receive.destinations
            });
            if same && receive.retained(links) {
                desired.remove(key);
                true
            } else {
                links.unbind_receive(receive.staged.binding);
                false
            }
        });
        for (key, target) in desired {
            let Some(sequence_anchor) = target.anchor else {
                continue;
            };
            let spec = crate::control::link::LiveReceiveSpec {
                peer,
                room: key.room,
                publication: target.publisher.publication,
                incarnation: target.publisher.incarnation,
                sequence_anchor,
                major: MAJOR,
                revision: target.revision,
                emission: target.publisher.emission,
                addresses: target.destinations.clone(),
            };
            if let Some(staged) = actor.stage_peer_live_receive(spec) {
                self.fabric_receives.insert(
                    key,
                    FabricReceive {
                        publisher: target.publisher,
                        revision: target.revision,
                        destinations: target.destinations,
                        staged,
                        acknowledged: false,
                    },
                );
            }
        }
    }

    fn reconcile_fabric_publishers(
        &mut self,
        actor: &mut ControllerActor,
        peer: NetworkId,
        now: Instant,
        authority: bool,
    ) {
        use crate::{control::link::PublisherSendPermit, route::fabric::PublisherSendChange};
        let mut desired = Vec::new();
        let mut deadline = now;
        if let Some(snapshot) = self
            .fabric_remote
            .as_ref()
            .filter(|snapshot| authority && now < snapshot.deadline)
        {
            deadline = snapshot.deadline;
            desired.extend(snapshot.routes.iter().filter_map(|&ack| {
                match ack.emission.profile() {
                    Profile::Data if ack.publication.kind() == TrackKind::Data => {}
                    Profile::Opus
                        if cfg!(feature = "sim")
                            && snapshot
                                .opus
                                .as_ref()
                                .is_some_and(|(revision, _)| *revision == ack.revision) => {}
                    _ => return None,
                }
                let (shard, publisher) = actor.lifecycle.network_publisher_anchor(
                    ack.room,
                    ack.publication,
                    &actor.core.registry,
                )?;
                (publisher.incarnation == ack.incarnation && publisher.emission == ack.emission)
                    .then_some(PublisherSendPermit {
                        shard,
                        ack,
                        publisher,
                    })
            }));
        }
        let Some(links) = &mut actor.links else {
            return;
        };
        let mut changes = Vec::new();
        self.publisher_pending
            .retain(|binding, receipt| match receipt.try_recv() {
                Err(oneshot::error::TryRecvError::Empty) => true,
                result => {
                    if result != Ok(true)
                        && let Some(change) = links.retire_publisher_send(*binding)
                    {
                        changes.push((*binding, change));
                    }
                    false
                }
            });
        changes.extend(links.reconcile_publisher_sends(peer, &desired, deadline, now));
        for (binding, change) in changes {
            match change {
                PublisherSendChange::Retire(_) => {
                    self.publisher_pending.remove(&binding);
                    actor.stage_publisher_retirement(binding, change);
                }
                PublisherSendChange::Install { .. } => {
                    if let Some(receipt) = actor.stage_publisher_send(change) {
                        self.publisher_pending.insert(binding, receipt);
                    } else if let Some(change) = actor
                        .links
                        .as_mut()
                        .and_then(|links| links.retire_publisher_send(binding))
                    {
                        actor.stage_publisher_retirement(binding, change);
                    }
                }
            }
        }
    }

    fn step_fabric(&mut self, actor: &mut ControllerActor, peer: NetworkId, now: Instant) {
        let authority = actor
            .receipt_trust
            .as_ref()
            .is_some_and(crate::control::controller::ownership::receipt::Authority::ready);
        self.reconcile_fabric_receives(actor, peer, now, authority);
        let (acknowledged, deadline) = self
            .fabric_remote
            .as_ref()
            .filter(|snapshot| authority && snapshot.deadline > now)
            .map_or((&[][..], now), |snapshot| {
                (snapshot.routes.as_ref(), snapshot.deadline)
            });
        let Some(links) = &mut actor.links else {
            return;
        };
        let mut rejected = Vec::new();
        self.fabric_pending
            .retain(|binding, receipt| match receipt.try_recv() {
                Err(oneshot::error::TryRecvError::Empty) => true,
                result => {
                    if result != Ok(true) {
                        rejected.push(*binding);
                    }
                    false
                }
            });
        for binding in rejected {
            links.retire_fabric_relay(binding);
        }
        for (binding, change) in links.reconcile_fabric_relays(peer, acknowledged, deadline, now) {
            if matches!(change, LiveRelayChange::Retire(..)) {
                self.fabric_pending.remove(&binding);
                actor.stage_fabric_relay_retirement(binding, change);
            } else if (self.fabric_pending.len() < LIMIT
                || self.fabric_pending.contains_key(&binding))
                && let Some(receipt) = actor.stage_fabric_relay_change(change)
            {
                self.fabric_pending.insert(binding, receipt);
            } else if let Some(links) = &mut actor.links {
                links.retire_fabric_relay(binding);
            }
        }
        self.reconcile_fabric_publishers(actor, peer, now, authority);
    }

    pub(super) fn step(&mut self, actor: &mut ControllerActor, peer: NetworkId, now: Instant) {
        let Some(links) = &mut actor.links else {
            return;
        };
        if links.fabric_peer(peer) {
            self.step_fabric(actor, peer, now);
            return;
        }
        let authority = actor
            .receipt_trust
            .as_ref()
            .is_some_and(crate::control::controller::ownership::receipt::Authority::ready);
        let mut available = if authority {
            links.relay_targets(peer, LIMIT).unwrap_or_default()
        } else {
            BTreeMap::new()
        };
        self.receives.retain(|key, receive| {
            if available.get(key) == Some(&receive.destinations) {
                available.remove(key);
                true
            } else {
                links.unbind_receive(receive.route.binding);
                false
            }
        });
        for (&key, destinations) in available.iter().filter(|(key, _)| key.reverse) {
            if let Ok(route) = links.bind_receive(peer, destinations.clone()) {
                self.receives.insert(
                    key,
                    Receive {
                        destinations: destinations.clone(),
                        route,
                    },
                );
            }
        }
        let mut desired = BTreeMap::new();
        if let Some((deadline, offers)) = &self.remote
            && authority
            && now < *deadline
        {
            for (&key, &label) in offers.iter().filter(|(key, _)| key.reverse) {
                let forward = Key {
                    reverse: false,
                    ..key
                };
                if let Some(destinations) = available.get(&forward) {
                    for dst in destinations {
                        let shard = dst.shard();
                        if actor.lifecycle.allows_ingress_reverse(
                            key.room,
                            key.publication(),
                            shard,
                            peer,
                        ) {
                            desired.insert((key, shard), label);
                        }
                    }
                }
            }
        }
        self.sends.retain(|key, (label, binding)| {
            if desired.get(key) == Some(label)
                && actor
                    .links
                    .as_ref()
                    .is_some_and(|links| links.send_live(*binding))
            {
                desired.remove(key);
                true
            } else {
                actor.retire_network_send(*binding);
                false
            }
        });
        for ((key, shard), label) in desired {
            let Some(links) = &mut actor.links else {
                break;
            };
            if let Ok((binding, plan)) = links.bind_reverse(shard, key.publication(), peer, label) {
                self.sends.insert((key, shard), (label, binding));
                let generation = actor.lifecycle.next_generation();
                actor.stage_plans_at(shard, generation, vec![plan]);
                actor.publish_staged();
            }
        }
    }
}
