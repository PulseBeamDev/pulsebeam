//! Association-local relay labels advertised from acknowledged shard routes.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use pulsebeam_core::fabric::MAJOR;
use pulsebeam_link::{BindingId, NetworkId, NodeRouteKey};
use pulsebeam_runtime::time::Instant;
use serde::{Deserialize, Serialize};

use super::ControllerActor;
use crate::{
    entity::{ProjectId, ProjectRoomKey, PublicationId, PublicationKey, RoomId, TrackKind},
    id::ShardId,
    link::ReceiveRoute,
    route::NodeRouteAddress,
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

#[derive(Default)]
pub(super) struct Relays {
    receives: BTreeMap<Key, Receive>,
    sends: BTreeMap<(Key, ShardId), (NodeRouteKey, BindingId)>,
    remote: Option<(Instant, Offers)>,
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

    pub(super) fn install(&mut self, offers: Offers, deadline: Instant) {
        self.remote = Some((deadline, offers));
    }

    pub(super) fn step(&mut self, actor: &mut ControllerActor, peer: NetworkId, now: Instant) {
        let Some(links) = &mut actor.links else {
            return;
        };
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
