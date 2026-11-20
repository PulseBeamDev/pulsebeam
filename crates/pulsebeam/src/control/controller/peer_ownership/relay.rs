//! Association-local relay labels advertised from acknowledged shard routes.

use std::collections::{BTreeMap, BTreeSet};

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

const LIMIT: usize = 128;

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
        for (key, destinations) in available {
            if let Ok(route) = links.bind_receive(peer, destinations.clone()) {
                self.receives.insert(
                    key,
                    Receive {
                        destinations,
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
            for (&key, &label) in offers {
                let publication = key.publication();
                if key.reverse {
                    let forward = Key {
                        reverse: false,
                        ..key
                    };
                    if let Some(receive) = self.receives.get(&forward) {
                        for dst in &receive.destinations {
                            let shard = dst.shard();
                            if actor.lifecycle.allows_ingress_reverse(
                                key.room,
                                publication,
                                shard,
                                peer,
                            ) {
                                desired.insert((key, shard), label);
                            }
                        }
                    }
                } else if let Some(shard) =
                    actor
                        .lifecycle
                        .network_source(key.room, publication, &actor.core.registry)
                {
                    desired.insert((key, shard), label);
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
            let bind = if key.reverse {
                links.bind_reverse(shard, key.publication(), peer, label)
            } else {
                links.bind_send(shard, key.publication(), peer, label)
            };
            if let Ok((binding, plan)) = bind {
                self.sends.insert((key, shard), (label, binding));
                let generation = actor.lifecycle.next_generation();
                actor.stage_plans_at(shard, generation, vec![plan]);
                actor.publish_staged();
            }
        }
    }
}
