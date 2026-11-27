//! Aggregate subscriber and child-edge demand onto one upstream receive per
//! source. A gateway sends through an installed receive child, not directly
//! from a signed track descriptor or an ACK from downstream.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    num::NonZeroU32,
    time::Duration,
};

use pulsebeam_core::fabric::{MAJOR, REGISTRY_REVISION, wan::Window};
use pulsebeam_link::NetworkId;
use pulsebeam_proto::state::Track;
use pulsebeam_runtime::time::Instant;

use crate::{
    control::{
        controller::{
            ControllerActor,
            ownership::observations::PublicationFact,
            peer_ownership::relay::{
                self, fabric::WanLiveRouteAck, wan_source::WanSourceWindow, wan_transit,
            },
        },
        link::{Links as MediaLinks, LiveReceiveSpec, wan::WanLiveReceiveSpec},
    },
    entity::{ConnectionId, EndpointKey, PublicationId, PublicationKey, TrackKind},
    route::fabric::LiveNetworkChild,
};

use super::{
    Connected, Member,
    forwarding::{Links, Router},
    frames::{Frame, HOPS, Request},
    media_branch::{self, Edge, Lease},
    media_client::Demand,
    media_permit, media_protection,
    media_read::Row,
};

const RENEW: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Ord, PartialOrd, Eq, PartialEq)]
struct Key(EndpointKey, TrackKind, PublicationId);

impl Key {
    fn new(endpoint: EndpointKey, publication: PublicationKey) -> Self {
        Self(endpoint, publication.kind(), publication.publication_id())
    }

    fn publication(self) -> PublicationKey {
        PublicationKey::new(self.1, self.2)
    }
}

struct Offer<'a> {
    source: WanSourceWindow,
    descriptor: &'a Track,
    permit: &'a [u8],
    issued_ms: u64,
    expires: Instant,
    cursor: Option<&'a Lease>,
}

struct Group<'a> {
    offer: Offer<'a>,
    local: Option<&'a Demand>,
    children: Vec<&'a Lease>,
    ambiguous: bool,
}

impl<'a> Group<'a> {
    fn add(&mut self, offer: Offer<'a>) {
        if self.offer.source.publisher != offer.source.publisher
            || self.offer.source.origin != offer.source.origin
            || self.offer.descriptor != offer.descriptor
        {
            self.ambiguous = true;
        } else if (self.offer.source.window_start, self.offer.issued_ms)
            < (offer.source.window_start, offer.issued_ms)
        {
            self.offer = offer;
        }
    }
}

pub(super) struct Plan<'a> {
    key: Key,
    offer: Offer<'a>,
    local: Option<&'a Demand>,
    children: Vec<&'a Lease>,
    upstream: Edge,
    pin_ingress: bool,
    journey: Journey,
}

struct Journey {
    border: Option<(Member, Member)>,
    visited: Vec<Member>,
    budget: u8,
    attempt: u8,
}

pub(super) fn plan<'a>(
    router: &Router<'_, Connected<'_>>,
    demands: impl Iterator<Item = &'a Demand>,
    leases: impl Iterator<Item = &'a Lease>,
    attempt_for: impl Fn(Member) -> u8,
    protect: bool,
) -> Vec<Plan<'a>> {
    let mut groups = BTreeMap::new();
    for demand in demands {
        let key = Key::new(demand.endpoint, demand.source.publisher.publication);
        let offer = Offer {
            source: demand.source,
            descriptor: &demand.descriptor,
            permit: &demand.permit,
            issued_ms: demand.issued_ms,
            expires: demand.expires,
            cursor: None,
        };
        match groups.entry(key) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(Group {
                    offer,
                    local: Some(demand),
                    children: Vec::new(),
                    ambiguous: false,
                });
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => entry.get_mut().add(offer),
        }
    }
    for lease in leases.filter(|lease| lease.source.origin != Some(router.local)) {
        let Some((endpoint, _)) =
            media_permit::source_name(lease.source.room.project, &lease.descriptor)
        else {
            continue;
        };
        let key = Key::new(endpoint, lease.source.publisher.publication);
        let offer = Offer {
            source: lease.source,
            descriptor: &lease.descriptor,
            permit: &lease.permit,
            issued_ms: lease.issued_ms,
            expires: lease.permit_deadline,
            cursor: Some(lease),
        };
        match groups.entry(key) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(Group {
                    offer,
                    local: None,
                    children: vec![lease],
                    ambiguous: false,
                });
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let group = entry.get_mut();
                group.add(offer);
                group.children.push(lease);
            }
        }
    }
    groups
        .into_iter()
        .filter_map(|(key, group)| {
            let Group {
                offer,
                local,
                mut children,
                ambiguous,
            } = group;
            if ambiguous {
                return None;
            }
            let target = offer.source.origin?;
            children.retain(|lease| offer.source.same_window(lease.source));
            let mut visited = offer
                .cursor
                .map_or_else(Vec::new, |lease| lease.visited.clone());
            visited.push(router.local);
            let budget = (HOPS + 1).checked_sub(u8::try_from(visited.len()).ok()?)?;
            let request = Request {
                target,
                border: offer.cursor.and_then(|lease| lease.border),
                budget,
                attempt: offer
                    .cursor
                    .map_or_else(|| attempt_for(target), |lease| lease.attempt),
                payload: &[],
            };
            let pin_ingress = protect
                && local.is_some()
                && children.is_empty()
                && router.tree.region(router.local) == router.tree.region(target)
                && media_branch::supports_protection(offer.source, offer.descriptor);
            let (upstream, border) = router.via(request, |member, request| {
                if visited.contains(&member)
                    || children.iter().any(|lease| lease.visited.contains(&member))
                {
                    return None;
                }
                let upstream = router.links.media_edge(router.links.link(member)?)?;
                Some((upstream, request.border))
            })?;
            let journey = Journey {
                border,
                visited,
                budget,
                attempt: request.attempt,
            };
            Some(Plan {
                key,
                offer,
                local,
                children,
                upstream,
                pin_ingress,
                journey,
            })
        })
        .take(relay::LIMIT)
        .collect()
}

struct Installed {
    source: WanSourceWindow,
    descriptor: Track,
    upstream: Edge,
    canonical: NetworkId,
    window: Option<Window>,
    viewer_only: bool,
    base_only: bool,
    drained: Option<Member>,
    next_renew: Instant,
    last_sent_permit: Option<u64>,
    announced: Option<Announcement>,
}

#[derive(Clone, Copy)]
struct Announcement {
    upstream: Edge,
    origin: Member,
    border: Option<(Member, Member)>,
    budget: u8,
    attempt: u8,
    ack: WanLiveRouteAck,
}

impl Installed {
    fn withdraw(&mut self, updates: &mut Vec<BranchUpdate>, now: Instant) {
        if let Some(route) = self.announced.take()
            && let Some(body) = media_branch::withdraw(route.ack)
            && let Some(expires) = now.checked_add(RENEW)
        {
            updates.push(BranchUpdate {
                route,
                body,
                expires,
            });
        }
    }
}

pub(super) struct BranchUpdate {
    route: Announcement,
    body: Box<[u8]>,
    expires: Instant,
}

impl BranchUpdate {
    pub(super) fn send(self, router: &Router<'_, Connected<'_>>) {
        let request = Request {
            target: self.route.origin,
            border: self.route.border,
            budget: self.route.budget,
            attempt: self.route.attempt,
            payload: &self.body,
        };
        if let Some(frame) = Frame::Branch(request).encode() {
            router
                .links
                .send(self.route.upstream.link, frame, self.expires);
        }
    }
}

type Outputs =
    HashMap<NetworkId, BTreeMap<NonZeroU32, (LiveNetworkChild, WanLiveRouteAck, Instant)>>;

#[derive(Default)]
pub(super) struct Receives {
    installed: BTreeMap<Key, Installed>,
    transit: HashMap<NetworkId, wan_transit::Sends>,
}

impl Receives {
    pub(super) fn transit_label_retired(&self, peer: NetworkId, label: NonZeroU32) -> bool {
        self.transit
            .get(&peer)
            .is_some_and(|sends| sends.retired(label))
    }

    pub(super) fn retire_remote_label(
        &self,
        links: &mut MediaLinks,
        upstream: Edge,
        label: NonZeroU32,
    ) {
        if let Some(window) = self.installed.values().find_map(|installed| {
            (installed.upstream == upstream)
                .then_some(installed.window)
                .flatten()
                .filter(|window| window.ssrc() == label)
        }) {
            links.retire_routed_wan_receive(upstream.network, window);
        }
    }

    pub(super) fn canonical_ingress(&self, demand: &Demand) -> Option<NetworkId> {
        let installed = self.installed.get(&Key::new(
            demand.endpoint,
            demand.source.publisher.publication,
        ))?;
        (installed.source.same_window(demand.source)
            && installed.source.origin == demand.source.origin)
            .then_some(installed.canonical)
    }

    pub(super) fn drained_relay(&self, demand: &Demand) -> Option<Member> {
        let installed = self.installed.get(&Key::new(
            demand.endpoint,
            demand.source.publisher.publication,
        ))?;
        installed
            .source
            .same_window(demand.source)
            .then_some(installed.drained)
            .flatten()
    }

    pub(super) fn drain_primary(
        &mut self,
        links: &mut MediaLinks,
        endpoint: EndpointKey,
        source: WanSourceWindow,
        upstream: Edge,
        relay: Member,
        now: Instant,
    ) -> Option<Vec<BranchUpdate>> {
        let installed = self
            .installed
            .get_mut(&Key::new(endpoint, source.publisher.publication))?;
        let window = installed.window?;
        if !installed.viewer_only
            || installed.upstream != upstream
            || !installed.source.same_window(source)
            || installed.source.origin != source.origin
            || installed.drained.is_some()
            || links
                .routed_wan_ack(upstream.network, window, now)
                .is_none()
        {
            return None;
        }
        installed.drained = Some(relay);
        installed.base_only = true;
        installed.window = None;
        links.retire_routed_wan_receive(upstream.network, window);
        let mut updates = Vec::new();
        installed.withdraw(&mut updates, now);
        Some(updates)
    }

    pub(super) fn live_media_grant(
        &self,
        peer: NetworkId,
        demand: super::super::peer_ownership::target_demand::Demand,
        now: Instant,
    ) -> Option<super::super::peer_ownership::media_repair::Grant> {
        self.transit
            .get(&peer)?
            .live_media_grant(peer, demand.label, demand.incarnation, now)
    }

    pub(super) fn media_repair_grant(
        &self,
        peer: NetworkId,
        demand: crate::control::controller::peer_ownership::media_repair::Demand,
        now: Instant,
    ) -> Option<crate::control::controller::peer_ownership::media_repair::Grant> {
        self.transit
            .get(&peer)?
            .media_repair_grant(peer, demand, now)
    }

    pub(super) fn step(
        &mut self,
        actor: &mut ControllerActor,
        plans: Vec<Plan<'_>>,
        protected: &media_protection::Receivers,
        now: Instant,
    ) -> Vec<BranchUpdate> {
        let mut next = BTreeMap::new();
        let mut updates = Vec::new();
        let mut outputs = Outputs::new();
        let Some(renew_at) = now.checked_add(RENEW) else {
            return updates;
        };
        for Plan {
            key,
            offer,
            local,
            children,
            upstream,
            pin_ingress,
            journey,
        } in plans
        {
            let source = offer.source;
            let Some(origin) = source.origin else {
                continue;
            };
            let mut old = self.installed.remove(&key);
            if old.as_ref().is_some_and(|old| {
                old.source.publisher != source.publisher
                    || old.source.origin != source.origin
                    || &old.descriptor != offer.descriptor
            }) && let Some(old) = old.take()
            {
                retire(actor, key, old, &mut updates, now);
            }
            let Some(track) =
                PublicationFact::routed_track(key.0, source.publisher, offer.descriptor)
            else {
                if let Some(old) = old {
                    retire(actor, key, old, &mut updates, now);
                }
                continue;
            };
            let meta = track.meta().clone();
            let receivers = local
                .into_iter()
                .flat_map(Demand::receivers)
                .collect::<HashSet<ConnectionId>>();
            let transit = (!children.is_empty()).then_some(wan_transit::SHARD);
            // The canonical peer scopes the shard route; it need not carry a
            // packet. Keep that scope while the signed publisher is unchanged.
            let canonical = old.as_ref().map_or(upstream.network, |old| old.canonical);
            if let Some(outcome) = actor.lifecycle.reconcile_routed_ingress(
                track,
                canonical,
                receivers,
                transit,
                &actor.core.registry,
                now,
            ) {
                actor.publish_track_lifecycle(outcome);
                actor.notify_scoped_source_changed(&meta);
            }
            if !actor.lifecycle.has_routed_ingress(
                key.0,
                source.publisher.publication,
                source.publisher.connection,
                canonical,
            ) {
                if let Some(old) = old {
                    retire(actor, key, old, &mut updates, now);
                }
                continue;
            }
            let mut installed = old.unwrap_or_else(|| Installed {
                source,
                descriptor: offer.descriptor.clone(),
                upstream,
                canonical,
                window: None,
                viewer_only: pin_ingress,
                base_only: false,
                drained: None,
                next_renew: now,
                last_sent_permit: None,
                announced: None,
            });
            if installed.upstream.link != upstream.link {
                installed.next_renew = now;
            }
            // A cold full receive can start mid-frame. Keep the warm base leg
            // carrying playback instead of splicing an arbitrary suffix into it.
            installed.viewer_only = pin_ingress;
            if !installed.source.same_window(source) {
                installed.drained = None;
            }
            installed.base_only = installed.drained.is_some()
                || pin_ingress
                    && (installed.base_only || installed.upstream != upstream)
                    && actor
                        .links
                        .as_ref()
                        .is_some_and(|links| protected.fresh_on(links, source, upstream, now));
            let window = receive(actor, &mut installed, source, upstream, offer.expires, now);
            installed.upstream = upstream;
            if installed.announced.as_ref().is_some_and(|old| {
                Some(old.ack.window) != window || old.upstream.link != upstream.link
            }) {
                installed.withdraw(&mut updates, now);
            }
            if let Some(window) = window {
                if let Some(links) = actor.links.as_ref() {
                    if links
                        .routed_wan_ack(upstream.network, window, now)
                        .is_some()
                        && let Some(track) = actor
                            .lifecycle
                            .source_exact(source.room, source.publisher.publication)
                    {
                        source.refresh_track_ladder(track, offer.expires, now);
                    }
                    if (now >= installed.next_renew
                        || installed.last_sent_permit != Some(offer.issued_ms))
                        && let Some(ack) = links.routed_wan_ack(upstream.network, window, now)
                        && let Some(body) = media_branch::encode(
                            Row::new(offer.descriptor, offer.permit.into()),
                            ack,
                            journey.visited.clone(),
                            None,
                        )
                    {
                        let route = Announcement {
                            upstream,
                            origin,
                            border: journey.border,
                            budget: journey.budget,
                            attempt: journey.attempt,
                            ack,
                        };
                        installed.announced = Some(route);
                        updates.push(BranchUpdate {
                            route,
                            body,
                            expires: offer.expires.min(renew_at),
                        });
                        installed.next_renew = renew_at;
                        installed.last_sent_permit = Some(offer.issued_ms);
                    }
                    if !children.is_empty()
                        && let Some((input, deadline)) = links.wan_transit_input(
                            upstream.network,
                            source,
                            wan_transit::SHARD,
                            true,
                            now,
                        )
                    {
                        for lease in children {
                            let network = lease.downstream.network;
                            if network != upstream.network {
                                outputs.entry(network).or_default().insert(
                                    lease.ack.window.ssrc(),
                                    (input, lease.ack, deadline.min(lease.permit_deadline)),
                                );
                            }
                        }
                    }
                }
            }
            next.insert(key, installed);
        }
        for (key, old) in std::mem::replace(&mut self.installed, next) {
            retire(actor, key, old, &mut updates, now);
        }
        for (peer, sends) in &mut self.transit {
            sends.reconcile(
                actor,
                *peer,
                outputs.remove(peer).unwrap_or_default(),
                now,
                true,
            );
        }
        // Labels stay consumed for the life of the association, even when no branch is active.
        self.transit.retain(|peer, _| {
            actor
                .links
                .as_ref()
                .is_some_and(|links| links.is_wan_peer(*peer))
        });
        for (peer, branches) in outputs {
            let mut sends = wan_transit::Sends::default();
            sends.reconcile(actor, peer, branches, now, true);
            if !sends.is_empty() {
                self.transit.insert(peer, sends);
            }
        }
        updates
    }
}

fn receive(
    actor: &mut ControllerActor,
    installed: &mut Installed,
    source: WanSourceWindow,
    upstream: Edge,
    expires: Instant,
    now: Instant,
) -> Option<Window> {
    let links = actor.links.as_mut()?;
    let addresses = links
        .relay_targets(installed.canonical, relay::LIMIT)
        .and_then(|mut routes| routes.remove(&source.scope()))
        .unwrap_or_default();
    if let Some(window) = installed.window {
        let same_source = installed.source.same_window(source);
        if same_source
            && !installed.base_only
            && installed.upstream.network == upstream.network
            && links.retain_routed_wan_receive(upstream.network, window, &addresses, expires, now)
        {
            return Some(window);
        }
        tracing::debug!(?window, ?upstream.network, same_source, targets = addresses.len(), "regional media receive replaced");
        links.retire_routed_wan_receive(installed.upstream.network, window);
        installed.window = None;
    }
    installed.source = source;
    installed.upstream = upstream;
    installed.next_renew = now;
    if installed.base_only || addresses.is_empty() {
        return None;
    }
    installed.window = actor.stage_peer_wan_live_receive(
        now,
        WanLiveReceiveSpec {
            live: LiveReceiveSpec {
                peer: upstream.network,
                room: source.room,
                publication: source.publisher.publication,
                incarnation: source.publisher.incarnation,
                sequence_anchor: source.sequence_anchor,
                major: MAJOR,
                revision: REGISTRY_REVISION,
                emission: source.publisher.emission,
                addresses,
            },
            canonical_ingress: installed.canonical,
            start: source.window_start,
            forms: source.forms(),
            offer_cut: None,
            regional: Some((upstream.link.member, source)),
            routed: true,
            protected: false,
            expires,
            timing: source.window_timing(),
        },
    );
    installed.window
}

fn retire(
    actor: &mut ControllerActor,
    key: Key,
    mut installed: Installed,
    updates: &mut Vec<BranchUpdate>,
    now: Instant,
) {
    installed.withdraw(updates, now);
    if let Some(window) = installed.window
        && let Some(links) = &mut actor.links
    {
        links.retire_routed_wan_receive(installed.upstream.network, window);
    }
    let endpoint = key.0;
    let publication = key.publication();
    let meta = actor
        .lifecycle
        .source_exact(endpoint.room, publication)
        .map(|track| track.meta().clone());
    if let Some(outcome) =
        actor
            .lifecycle
            .withdraw_routed_ingress(endpoint, publication, &actor.core.registry, now)
    {
        actor.publish_track_lifecycle(outcome);
        if let Some(meta) = meta {
            actor.notify_scoped_source_changed(&meta);
        }
    }
}
