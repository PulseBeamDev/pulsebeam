//! Association-local relay labels advertised from acknowledged shard routes.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use pulsebeam_core::fabric::MAJOR;
use pulsebeam_link::{NetworkId, NodeRouteKey};
use pulsebeam_runtime::time::Instant;
use serde::{Deserialize, Serialize};

use super::ControllerActor;
use crate::{
    entity::{ProjectId, ProjectRoomKey, PublicationId, PublicationKey, RoomId, TrackKind},
    route::NodeRouteAddress,
};

pub(in crate::control) mod fabric;
pub(in crate::control::controller) mod wan_publisher;
pub(super) mod wan_repair;
pub(in crate::control) mod wan_source;
pub(in crate::control::controller) mod wan_transit;

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

pub(super) fn reconcile_wan_receives(
    actor: &mut ControllerActor,
    peer: NetworkId,
    session: super::Session,
    regional_peer: Option<crate::cluster::mesh::Member>,
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
            let selection = if let Some(regional_peer) = regional_peer {
                source.offer_cut?;
                let selected = *actor
                    .regional_mesh
                    .as_ref()?
                    .path(source.origin?)?
                    .first()?;
                Some((selected, regional_peer == selected))
            } else {
                None
            };
            let publication = source.publisher.publication;
            if actor
                .lifecycle
                .uses_routed_receive(source.room, publication)
            {
                return None;
            }
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
            let addresses = routes.as_ref()?.get(&source.scope())?;
            let may_stage = selection.is_none_or(|(_, selected)| selected);
            // The held route chooses where to stage, not when to withdraw the
            // incumbent. An install ACK alone does not prove remote sending.
            if let Some((selected, false)) = selection
                && links.regional_incumbent(source, canonical, addresses, selected, now)
                    != Some(peer)
            {
                return None;
            }
            Some((source, canonical, addresses.clone(), may_stage))
        }));
    }
    links.retain_wan_live_receives(peer, |ack, canonical, children, regional| {
        if let Some(index) = desired
            .iter()
            .position(|(source, selected, destinations, _)| {
                *selected == canonical
                    && source.matches_ack(ack)
                    && regional.is_none_or(|offered| {
                        offered.origin == source.origin && offered.same_window(*source)
                    })
                    && children
                        .iter()
                        .map(|child| child.destination)
                        .eq(destinations.iter().copied())
            })
        {
            let (source, _, _, _) = desired.swap_remove(index);
            if canonical == peer
                && regional_peer.is_none_or(|member| source.origin == Some(member))
                && let Some(track) = actor
                    .lifecycle
                    .source_exact(source.room, source.publisher.publication)
            {
                source.refresh_track_ladder(track, expires, now);
            }
            true
        } else {
            false
        }
    });
    desired.retain(|(_, _, _, may_stage)| *may_stage);
    for (source, canonical_ingress, addresses, _) in desired {
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
                offer_cut: source.offer_cut,
                regional: regional_peer.map(|member| (member, source)),
                routed: false,
                expires,
                timing: source.window_timing(),
            },
        );
    }
}
