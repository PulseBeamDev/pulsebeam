use std::{
    collections::BTreeMap,
    num::{NonZeroU32, NonZeroU64},
    task::Poll,
    time::Duration,
};

use arrayvec::ArrayVec;
use pulsebeam_core::fabric::{MAJOR, REGISTRY_REVISION};
use pulsebeam_link::{BindingCutoff, NetworkId};
use pulsebeam_proto::state::Track;
use pulsebeam_runtime::time::Instant;

use crate::{
    control::{
        controller::{
            ControllerActor,
            peer_ownership::{
                media_repair,
                relay::{self, fabric::WanLiveRouteAck, wan_source::WanSourceWindow},
            },
        },
        link::{Links as MediaLinks, LiveReceiveSpec, wan::WanLiveReceiveSpec},
    },
    link::wan_host::{ParentSend, PendingTx, WanTx, WarmReceive},
    route::NodeRouteAddress,
};

use super::{
    Connected, Member, Router,
    forwarding::Links,
    frames::{Frame, HOPS, Request},
    media_branch::{self, Edge, Lease, ProtectionPath},
    media_client::Demand,
    media_ingress,
    media_read::Row,
};

const RENEW: Duration = Duration::from_millis(250);
const MEDIA_FRESH: Duration = Duration::from_millis(500);

#[derive(Clone, Copy)]
struct Permit<'a> {
    source: WanSourceWindow,
    descriptor: &'a Track,
    signed: &'a [u8],
    issued_ms: u64,
    expires: Instant,
}

impl<'a> From<&'a Lease> for Permit<'a> {
    fn from(lease: &'a Lease) -> Self {
        Self {
            source: lease.source,
            descriptor: &lease.descriptor,
            signed: &lease.permit,
            issued_ms: lease.issued_ms,
            expires: lease.expires,
        }
    }
}

impl<'a> From<&'a Demand> for Permit<'a> {
    fn from(demand: &'a Demand) -> Self {
        Self {
            source: demand.source,
            descriptor: &demand.descriptor,
            signed: &demand.permit,
            issued_ms: demand.issued_ms,
            expires: demand.expires,
        }
    }
}

#[derive(PartialEq, Eq)]
struct Hop {
    upstream: Edge,
    path: ProtectionPath,
}

pub(super) struct TransitPlan<'a> {
    lease: &'a Lease,
    hop: Hop,
    visited: Vec<Member>,
}

impl<'a> TransitPlan<'a> {
    pub(super) fn new(router: &Router<'_, Connected<'_>>, lease: &'a Lease) -> Option<Self> {
        let path = lease.protection.as_ref()?;
        let next = path.next(router.local)?;
        let downstream = router
            .links
            .media_edge(router.links.link(lease.downstream.link.member)?)?;
        if downstream != lease.downstream {
            return None;
        }
        let upstream = router.links.media_edge(router.links.link(next)?)?;
        if upstream.network == downstream.network {
            return None;
        }
        let mut visited = lease.visited.clone();
        visited.push(router.local);
        Some(Self {
            lease,
            hop: Hop {
                upstream,
                path: path.clone(),
            },
            visited,
        })
    }
}

#[derive(Default)]
pub(super) struct Transit {
    legs: BTreeMap<(Member, NonZeroU32), Relay>,
}

struct Relay {
    downstream: Edge,
    child: WanLiveRouteAck,
    input: Input,
    output: Option<Output>,
}

struct Output {
    cutoff: BindingCutoff,
    phase: OutputPhase,
}

enum OutputPhase {
    Pending(PendingTx),
    Attached(WanTx),
}

struct Input<T = WarmReceive> {
    source: WanSourceWindow,
    descriptor: Track,
    hop: Hop,
    ack: WanLiveRouteAck,
    receive: T,
    next_renew: Instant,
    last_sent_permit: Option<u64>,
    announcement: Option<Announcement>,
}

#[derive(Clone, Copy)]
struct Announcement {
    upstream: Edge,
    origin: Member,
    budget: u8,
}

pub(super) struct Update {
    route: Announcement,
    body: Box<[u8]>,
    expires: Instant,
}

impl Update {
    pub(super) fn send(self, router: &Router<'_, Connected<'_>>) {
        if let Some(frame) = Frame::Branch(Request {
            target: self.route.origin,
            border: None,
            budget: self.route.budget,
            attempt: 0,
            payload: &self.body,
        })
        .encode()
        {
            router
                .links
                .send(self.route.upstream.link, frame, self.expires);
        }
    }
}

impl Input {
    fn stage(links: &mut MediaLinks, permit: Permit<'_>, hop: Hop, now: Instant) -> Option<Self> {
        let (ack, receive) = links
            .stage_protected_wan_receive(hop.upstream.network, permit.source, permit.expires)
            .ok()?;
        Some(Self::new(permit, hop, ack, receive, now))
    }

    fn refresh(&self, permit: Permit<'_>, now: Instant) -> Option<bool> {
        if now >= permit.expires || !self.receive.available(now) {
            return None;
        }
        if self.receive.installed(now) != Some(self.ack.window) {
            return Some(false);
        }
        self.receive.renew(now, permit.expires).then_some(true)
    }
}

impl<T> Input<T> {
    fn new(permit: Permit<'_>, hop: Hop, ack: WanLiveRouteAck, receive: T, now: Instant) -> Self {
        Self {
            source: permit.source,
            descriptor: permit.descriptor.clone(),
            hop,
            ack,
            receive,
            next_renew: now,
            last_sent_permit: None,
            announcement: None,
        }
    }

    fn matches(&self, permit: Permit<'_>, hop: &Hop) -> bool {
        self.hop == *hop
            && self.descriptor == *permit.descriptor
            && self.source.same_window(permit.source)
            && permit.source.matches_ack(self.ack)
    }

    fn announce(
        &mut self,
        permit: Permit<'_>,
        visited: &[Member],
        now: Instant,
        updates: &mut Vec<Update>,
    ) -> Option<()> {
        if now < self.next_renew && self.last_sent_permit == Some(permit.issued_ms) {
            return Some(());
        }
        let renew_at = now.checked_add(RENEW)?;
        let route = Announcement {
            upstream: self.hop.upstream,
            origin: permit.source.origin?,
            budget: (HOPS + 1).checked_sub(u8::try_from(visited.len()).ok()?)?,
        };
        let body = media_branch::encode(
            Row::new(permit.descriptor, permit.signed.into()),
            self.ack,
            visited.to_vec(),
            Some(self.hop.path.clone()),
        )?;
        self.announcement = Some(route);
        self.next_renew = renew_at;
        self.last_sent_permit = Some(permit.issued_ms);
        updates.push(Update {
            route,
            body,
            expires: permit.expires.min(renew_at),
        });
        Some(())
    }

    fn withdraw(self, now: Instant, updates: &mut Vec<Update>) {
        if let (Some(route), Some(body), Some(expires)) = (
            self.announcement,
            media_branch::withdraw(self.ack),
            now.checked_add(RENEW),
        ) {
            updates.push(Update {
                route,
                body,
                expires,
            });
        }
    }
}

impl Transit {
    pub(super) fn media_repair_grant(
        &self,
        peer: NetworkId,
        demand: media_repair::Demand,
        now: Instant,
    ) -> Option<media_repair::Grant> {
        let leg = self.legs.values().find(|leg| {
            leg.downstream.network == peer
                && leg.child.incarnation == demand.source
                && demand.matches_parent(leg.child.window)
        })?;
        let output = leg.output.as_ref()?;
        let OutputPhase::Attached(tx) = &output.phase else {
            return None;
        };
        let expires = tx.expires();
        if now >= expires
            || !tx.is_live()
            || !output.cutoff.is_live()
            || leg.input.receive.installed(now) != Some(leg.input.ack.window)
        {
            return None;
        }
        Some(media_repair::Grant {
            path: media_repair::Source::Warm(leg.input.receive.fence()),
            parent: ParentSend {
                cutoff: output.cutoff.clone(),
                window: leg.child.window,
            },
            expires,
        })
    }

    pub(super) fn step(
        &mut self,
        links: &mut MediaLinks,
        plans: Vec<TransitPlan<'_>>,
        now: Instant,
    ) -> Vec<Update> {
        let mut next = BTreeMap::new();
        let mut updates = Vec::new();
        for TransitPlan {
            lease,
            hop,
            visited,
        } in plans
        {
            let permit = Permit::from(lease);
            let key = (lease.downstream.link.member, lease.ack.window.ssrc());
            let old = self.legs.remove(&key);
            let mut leg = match old {
                Some(old)
                    if old.downstream == lease.downstream
                        && old.child == lease.ack
                        && old.input.matches(permit, &hop) =>
                {
                    old
                }
                old => {
                    if let Some(old) = old {
                        old.retire(links, now, &mut updates);
                    }
                    let Some(input) = Input::stage(links, permit, hop, now) else {
                        continue;
                    };
                    Relay {
                        downstream: lease.downstream,
                        child: lease.ack,
                        input,
                        output: None,
                    }
                }
            };
            if leg
                .advance(links, permit, &visited, now, &mut updates)
                .is_some()
            {
                next.insert(key, leg);
            } else {
                leg.retire(links, now, &mut updates);
            }
        }
        for (_, old) in std::mem::replace(&mut self.legs, next) {
            old.retire(links, now, &mut updates);
        }
        updates
    }
}

impl Relay {
    fn advance(
        &mut self,
        links: &mut MediaLinks,
        permit: Permit<'_>,
        visited: &[Member],
        now: Instant,
        updates: &mut Vec<Update>,
    ) -> Option<()> {
        if !self.input.refresh(permit, now)? {
            return Some(());
        }
        if self.output.is_none() {
            let (cutoff, pending) = links
                .stage_protected_wan_transit(
                    self.downstream.network,
                    &self.input.receive,
                    self.child,
                    self.input.hop.path.wire_bps(),
                    permit.expires,
                    now,
                )
                .ok()?;
            self.output = Some(Output {
                cutoff,
                phase: OutputPhase::Pending(pending),
            });
        }
        let output = self.output.as_mut()?;
        if let OutputPhase::Pending(pending) = &mut output.phase {
            match pending.poll() {
                Poll::Pending => return Some(()),
                Poll::Ready(Some(tx)) => {
                    links
                        .forward_protected_wan_receive(&self.input.receive, tx.clone())
                        .ok()?;
                    output.phase = OutputPhase::Attached(tx);
                }
                Poll::Ready(None) => return None,
            }
        }
        let OutputPhase::Attached(tx) = &output.phase else {
            return None;
        };
        tx.renew(now, permit.expires).then_some(())?;
        self.input.announce(permit, visited, now, updates)
    }

    fn retire(self, links: &mut MediaLinks, now: Instant, updates: &mut Vec<Update>) {
        if let Some(output) = self.output {
            links.retire_wan_send(&output.cutoff);
        }
        self.input.withdraw(now, updates);
    }
}

pub(super) struct ReceiverPlan<'a> {
    pub(super) demand: &'a Demand,
    permit: Permit<'a>,
    hops: ArrayVec<Hop, 2>,
}

impl<'a> ReceiverPlan<'a> {
    pub(super) fn new(
        router: &Router<'_, Connected<'_>>,
        demand: &'a Demand,
        wire_bps: NonZeroU64,
        now: Instant,
    ) -> Option<Self> {
        let permit = Permit::from(demand);
        let origin = permit.source.origin?;
        if now >= permit.expires
            || router.tree.region(origin)? != router.tree.region(router.local)?
            || !media_branch::supports_protection(permit.source, permit.descriptor)
        {
            return None;
        }
        let mesh = router.links.mesh?;
        let hop = |path: &[Member]| {
            let upstream = router
                .links
                .media_edge(router.links.link(*path.first()?)?)?;
            let path = ProtectionPath::new(
                std::iter::once(router.local)
                    .chain(path.iter().copied())
                    .collect(),
                wire_bps,
            )?;
            Some(Hop { upstream, path })
        };
        let primary = hop(mesh.path(origin)?);
        // A media adjacency can outlive its control stream. Don't let an
        // unusable computed primary invalidate the exact, still-live standby.
        let standby = mesh
            .protection_paths(origin, now)
            .and_then(|(_, path)| hop(&path))
            .filter(|standby| {
                primary
                    .as_ref()
                    .is_none_or(|primary| standby.upstream.network != primary.upstream.network)
            });
        let mut hops = ArrayVec::new();
        hops.extend(primary);
        hops.extend(standby);
        (!hops.is_empty()).then_some(Self {
            demand,
            permit,
            hops,
        })
    }
}

pub(super) fn receiver_plans<'a>(
    router: &Router<'_, Connected<'_>>,
    demands: impl Iterator<Item = &'a Demand>,
    wire_bps: NonZeroU64,
    now: Instant,
) -> BTreeMap<relay::Key, ReceiverPlan<'a>> {
    let mut sources = BTreeMap::new();
    for demand in demands {
        sources
            .entry(demand.source.scope())
            .and_modify(|demand| *demand = None)
            .or_insert(Some(demand));
    }
    sources
        .into_iter()
        .filter_map(|(key, demand)| Some((key, ReceiverPlan::new(router, demand?, wire_bps, now)?)))
        .take(relay::LIMIT)
        .collect()
}

#[derive(Default)]
pub(super) struct Receivers {
    routes: BTreeMap<relay::Key, ReceiveLegs>,
}

struct ReceiveLegs {
    canonical: NetworkId,
    legs: ArrayVec<Input<()>, 2>,
}

impl Input<()> {
    fn stage_routed(
        actor: &mut ControllerActor,
        permit: Permit<'_>,
        hop: Hop,
        canonical: NetworkId,
        addresses: Vec<NodeRouteAddress>,
        now: Instant,
    ) -> Option<Self> {
        let source = permit.source;
        let window = actor.stage_peer_wan_live_receive(
            now,
            WanLiveReceiveSpec {
                live: LiveReceiveSpec {
                    peer: hop.upstream.network,
                    room: source.room,
                    publication: source.publisher.publication,
                    incarnation: source.publisher.incarnation,
                    sequence_anchor: source.sequence_anchor,
                    major: MAJOR,
                    revision: REGISTRY_REVISION,
                    emission: source.publisher.emission,
                    addresses,
                },
                canonical_ingress: canonical,
                start: source.window_start,
                forms: source.forms(),
                offer_cut: source.offer_cut,
                regional: Some((hop.upstream.link.member, source)),
                routed: true,
                protected: true,
                expires: permit.expires,
                timing: source.window_timing(),
            },
        )?;
        let ack = WanLiveRouteAck {
            room: source.room,
            publication: source.publisher.publication,
            incarnation: source.publisher.incarnation,
            sequence_anchor: source.sequence_anchor,
            offer_cut: source.offer_cut,
            window,
        };
        Some(Self::new(permit, hop, ack, (), now))
    }

    fn refresh_routed(
        &self,
        links: &MediaLinks,
        addresses: &[NodeRouteAddress],
        expires: Instant,
        now: Instant,
    ) -> Option<bool> {
        let peer = self.hop.upstream.network;
        let window = self.ack.window;
        links
            .retain_routed_wan_receive(peer, window, addresses, expires, now)
            .then(|| links.routed_wan_ack(peer, window, now) == Some(self.ack))
    }

    fn fresh_media(&self, links: &MediaLinks, now: Instant) -> bool {
        links
            .protected_wan_last_media(self.hop.upstream.network, self.ack.window, now)
            .and_then(|received| now.checked_duration_since(received))
            .is_some_and(|age| age < MEDIA_FRESH)
    }

    fn retire_routed(self, links: &mut MediaLinks, now: Instant, updates: &mut Vec<Update>) {
        links.retire_routed_wan_receive(self.hop.upstream.network, self.ack.window);
        self.withdraw(now, updates);
    }
}

impl ReceiveLegs {
    fn retire(self, actor: &mut ControllerActor, now: Instant, updates: &mut Vec<Update>) {
        if let Some(links) = actor.links.as_mut() {
            for input in self.legs {
                input.retire_routed(links, now, updates);
            }
        }
    }
}

impl Receivers {
    pub(super) fn drainable_primary(
        &self,
        links: &MediaLinks,
        plan: &ReceiverPlan<'_>,
        relay: Member,
        now: Instant,
    ) -> Option<Edge> {
        let [primary, standby] = plan.hops.as_slice() else {
            return None;
        };
        (primary.path.includes_relay(relay)
            && !standby.path.includes_relay(relay)
            && self.protected(links, plan, now))
        .then_some(primary.upstream)
    }

    pub(super) fn retire_relay(
        &mut self,
        links: &mut MediaLinks,
        source: WanSourceWindow,
        relay: Member,
        now: Instant,
    ) -> Vec<Update> {
        let mut updates = Vec::new();
        if let Some(state) = self.routes.get_mut(&source.scope()) {
            for index in (0..state.legs.len()).rev() {
                if state.legs[index].hop.path.includes_relay(relay) {
                    state
                        .legs
                        .remove(index)
                        .retire_routed(links, now, &mut updates);
                }
            }
        }
        updates
    }

    pub(super) fn alternate_repair_parent(
        &self,
        links: &MediaLinks,
        plan: &ReceiverPlan<'_>,
        original: crate::route::fabric::LiveNetworkChild,
        position: u64,
        now: Instant,
    ) -> Option<crate::route::fabric::LiveNetworkChild> {
        if !self.protected(links, plan, now) {
            return None;
        }
        let state = self.routes.get(&plan.permit.source.scope())?;
        let [left, right] = state.legs.as_slice() else {
            return None;
        };
        let matches = |leg: &Input<()>| {
            leg.hop.upstream.network == original.ingress.association
                && Some(leg.ack.window) == original.window
        };
        let other = match (matches(left), matches(right)) {
            (true, false) => right,
            (false, true) => left,
            _ => return None,
        };
        if !other.ack.window.contains(position) {
            return None;
        }
        links.protected_wan_child(
            other.hop.upstream.network,
            other.ack,
            original.destination,
            now,
        )
    }

    pub(super) fn fresh_on(
        &self,
        links: &MediaLinks,
        source: WanSourceWindow,
        edge: Edge,
        now: Instant,
    ) -> bool {
        self.routes.get(&source.scope()).is_some_and(|state| {
            state.legs.iter().any(|leg| {
                leg.source.origin == source.origin
                    && leg.source.same_window(source)
                    && leg.hop.upstream == edge
                    && leg.fresh_media(links, now)
            })
        })
    }

    pub(super) fn protected(
        &self,
        links: &MediaLinks,
        plan: &ReceiverPlan<'_>,
        now: Instant,
    ) -> bool {
        plan.hops.len() == 2
            && self
                .routes
                .get(&plan.permit.source.scope())
                .is_some_and(|state| {
                    plan.hops.iter().all(|hop| {
                        state
                            .legs
                            .iter()
                            .any(|leg| leg.matches(plan.permit, hop) && leg.fresh_media(links, now))
                    })
                })
    }

    pub(super) fn step(
        &mut self,
        actor: &mut ControllerActor,
        primary: &media_ingress::Receives,
        local: Member,
        plans: BTreeMap<relay::Key, ReceiverPlan<'_>>,
        now: Instant,
    ) -> Vec<Update> {
        let mut updates = Vec::new();
        let mut next = BTreeMap::new();
        for (key, mut plan) in plans {
            let permit = plan.permit;
            let drained = primary.drained_relay(plan.demand);
            if let Some(relay) = drained {
                plan.hops.retain(|hop| !hop.path.includes_relay(relay));
            }
            let Some(canonical) = primary.canonical_ingress(plan.demand) else {
                continue;
            };
            let Some(links) = actor.links.as_mut() else {
                break;
            };
            let Some(addresses) = links
                .relay_targets(canonical, relay::LIMIT)
                .and_then(|mut targets| targets.remove(&key))
            else {
                continue;
            };
            let mut legs = ArrayVec::new();
            if let Some(old) = self.routes.remove(&key) {
                for mut leg in old.legs {
                    let retained = old.canonical == canonical
                        && plan.hops.iter().any(|hop| leg.matches(permit, hop))
                        && match leg.refresh_routed(links, &addresses, permit.expires, now) {
                            Some(true) => {
                                leg.announce(permit, &[local], now, &mut updates).is_some()
                            }
                            Some(false) => true,
                            None => false,
                        };
                    if retained {
                        legs.push(leg);
                    } else {
                        leg.retire_routed(links, now, &mut updates);
                    }
                }
            }
            // Never open protection on an initially unprotected single path.
            // Once a pair exists, keep a matching, still-authorized survivor.
            if plan.hops.len() == 2 && drained.is_none() {
                for hop in plan.hops {
                    if legs.iter().any(|leg| leg.hop == hop) {
                        continue;
                    }
                    if let Some(leg) =
                        Input::stage_routed(actor, permit, hop, canonical, addresses.clone(), now)
                    {
                        legs.push(leg);
                    }
                }
            }
            if !legs.is_empty() {
                next.insert(key, ReceiveLegs { canonical, legs });
            }
        }
        for (_, old) in std::mem::replace(&mut self.routes, next) {
            old.retire(actor, now, &mut updates);
        }
        updates
    }
}
