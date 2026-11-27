//! Bounded catalog and media owner reads over the region tree. Routing membership and border
//! sessions are independent of publisher issuers and ownership observations.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    num::NonZeroU64,
    time::SystemTime,
};

use pulsebeam_core::{auth::ApiSigningKey, identity::NodeId};
use pulsebeam_link::{LinkTime, NetworkId, NetworkPeer, PeerControl};
use pulsebeam_runtime::{sync::mpsc, time::Instant};

use crate::{
    clock::reference::Snapshot,
    cluster::{
        RegionTree,
        admission::{AdmittedBoot, BootSession, DiscoveryPort, MediaCredentials},
        boot_announcement,
        mesh::{Exchange, Member, Port},
    },
};

use super::{
    ControllerActor,
    catalog::{
        neighbor::{
            self, Get, MessageOnStream,
            client::{Order, Reads, Window},
        },
        responders::DeclarationId,
    },
    peer_ownership::provisioned::Peer,
};

mod discovery;
mod forwarding;
mod frames;
mod media_branch;
mod media_client;
mod media_ingress;
mod media_permit;
mod media_protection;
mod media_read;

use forwarding::{Arrival, Forwarding, Link, Links, Router};
pub(super) fn recognizes(frame: &[u8]) -> bool {
    frames::recognizes(frame) || discovery::recognizes(frame)
}

const INBOX_CAPACITY: usize = 64;
const DISCOVERY_INBOX_CAPACITY: usize = 16;
const BORDER_QUANTUM: usize = 4;
const ADMISSION_QUANTUM: usize = 16;

// A routed read is anchored to the target member and the owner's row revision,
// not to a declaration or stream belonging to a replaceable next hop.
pub(super) const ROUTED: DeclarationId = DeclarationId {
    stream: NonZeroU64::MIN,
    interest: NonZeroU64::MIN,
    changed_at: NonZeroU64::MIN,
};

struct EnrolledBorder {
    transport: MediaCredentials,
    host: crate::link::wan_host::Provisioner,
    current: Option<Member>,
}

#[derive(Default)]
struct Border {
    port: Option<Port>,
    lane: Option<PeerControl>,
    revision: u64,
    media: Option<NetworkId>,
}

impl Border {
    fn advance(&mut self) {
        if self.lane.as_ref().is_some_and(PeerControl::is_ready) {
            return;
        }
        self.lane = None;
        if let Some(port) = &mut self.port
            && let Ok(lane) = port.sessions.try_recv()
        {
            self.install_lane(lane);
        }
    }

    fn install_lane(&mut self, lane: PeerControl) {
        if lane.is_ready()
            && let Some(next) = self.revision.checked_add(1)
        {
            self.revision = next;
            self.lane = Some(lane);
        }
    }

    fn live(&self) -> Option<(NonZeroU64, &PeerControl)> {
        let lane = self.lane.as_ref().filter(|lane| lane.is_ready())?;
        Some((NonZeroU64::new(self.revision)?, lane))
    }
}

struct Connected<'a> {
    epoch: Instant,
    now: Instant,
    mesh: Option<&'a Exchange>,
    peers: &'a [Peer],
    borders: &'a BTreeMap<Member, Border>,
}

impl Connected<'_> {
    fn discovery_links(&self) -> Vec<Link> {
        self.borders
            .keys()
            .copied()
            .chain(self.peers.iter().map(Peer::regional_member))
            .filter_map(|member| self.link(member))
            .collect()
    }

    fn media_edge(&self, link: Link) -> Option<media_branch::Edge> {
        let network = self
            .borders
            .get(&link.member)
            .and_then(|border| border.media)
            .or_else(|| {
                self.peers
                    .iter()
                    .find_map(|peer| peer.routed_media(link.member))
            })?;
        Some(media_branch::Edge { link, network })
    }

    fn lane(&self, target: Member) -> Option<(NonZeroU64, &PeerControl)> {
        self.borders
            .get(&target)
            .and_then(Border::live)
            .or_else(|| {
                self.peers
                    .iter()
                    .find_map(|peer| peer.routed_control(target))
            })
    }
}

impl Links for Connected<'_> {
    fn first_local_hop(&self, destination: Member) -> Option<Member> {
        let mesh = self.mesh?;
        let primary = *mesh.path(destination)?.first()?;
        if self.lane(primary).is_some() {
            return Some(primary);
        }
        // The installed media path can drain while its control lane is closed.
        // Route new control requests over a live, independent first hop.
        let (_, alternate) = mesh.protection_paths(destination, self.now)?;
        alternate
            .first()
            .copied()
            .filter(|hop| self.lane(*hop).is_some())
    }

    fn link(&self, member: Member) -> Option<Link> {
        Some(Link {
            member,
            stream: self.lane(member)?.0,
        })
    }

    fn send(&self, link: Link, bytes: Box<[u8]>, expires: Instant) -> bool {
        let Some((stream, lane)) = self.lane(link.member) else {
            return false;
        };
        let expiry = expires.saturating_duration_since(self.epoch);
        stream == link.stream
            && u64::try_from(expiry.as_nanos())
                .is_ok_and(|deadline| lane.try_send(bytes, LinkTime::from_nanos(deadline)).is_ok())
    }
}

pub(super) struct Routing {
    local: Member,
    tree: RegionTree,
    epoch: Instant,
    borders: BTreeMap<Member, Border>,
    enrolled_borders: BTreeMap<NodeId, EnrolledBorder>,
    admitted_boots: Option<mpsc::Receiver<AdmittedBoot>>,
    discovery: Option<discovery::Discovery>,
    discovery_inbox: VecDeque<(Link, Box<[u8]>)>,
    inbox: VecDeque<(Link, Box<[u8]>)>,
    forwarding: Forwarding,
    reads: BTreeMap<Member, Reads>,
    media_key: Option<ApiSigningKey>,
    media_owner: media_read::OwnerReads,
    media_targets: Vec<Member>,
    media_client: media_client::Client,
    media_branches: media_branch::Leases,
    media_sends: media_branch::OriginSends,
    media_receives: media_ingress::Receives,
    protected_transit: media_protection::Transit,
    protected_base_stream_bps: Option<NonZeroU64>,
    protected_receivers: media_protection::Receivers,
}

impl Routing {
    pub(super) fn protected_media(
        &self,
        actor: &ControllerActor,
        room: crate::entity::ProjectRoomKey,
        publication: crate::entity::PublicationKey,
        now: Instant,
    ) -> bool {
        self.protection_plan(actor, room, publication, now)
            .zip(actor.links.as_ref())
            .is_some_and(|(plan, links)| self.protected_receivers.protected(links, &plan, now))
    }

    fn protection_plan(
        &self,
        actor: &ControllerActor,
        room: crate::entity::ProjectRoomKey,
        publication: crate::entity::PublicationKey,
        now: Instant,
    ) -> Option<media_protection::ReceiverPlan<'_>> {
        let wire_bps = self.protected_base_stream_bps?;
        let wall = SystemTime::now();
        let mut demands = self.media_client.demands().filter(|demand| {
            demand.source.room == room
                && demand.source.publisher.publication == publication
                && demand.authorized(actor, now, wall)
        });
        let demand = demands.next()?;
        if demands.next().is_some() {
            return None;
        }
        let router = Router {
            local: self.local,
            tree: &self.tree,
            links: Connected {
                epoch: self.epoch,
                now,
                mesh: actor.regional_mesh.as_ref(),
                peers: &actor.provisioned_peers,
                borders: &self.borders,
            },
        };
        media_protection::ReceiverPlan::new(&router, demand, wire_bps, now)
    }

    pub(super) fn drain_media_relay(
        &mut self,
        actor: &mut ControllerActor,
        room: crate::entity::ProjectRoomKey,
        publication: crate::entity::PublicationKey,
        relay: Member,
        now: Instant,
    ) -> bool {
        let Some((plan, links)) = self
            .protection_plan(actor, room, publication, now)
            .zip(actor.links.as_ref())
        else {
            return false;
        };
        let Some(primary) = self
            .protected_receivers
            .drainable_primary(links, &plan, relay, now)
        else {
            return false;
        };
        let (endpoint, source) = (plan.demand.endpoint, plan.demand.source);
        let Some(links) = actor.links.as_mut() else {
            return false;
        };
        let Some(withdrawals) = self
            .media_receives
            .drain_primary(links, endpoint, source, primary, relay, now)
        else {
            return false;
        };
        let protected = self
            .protected_receivers
            .retire_relay(links, source, relay, now);
        let router = Router {
            local: self.local,
            tree: &self.tree,
            links: Connected {
                epoch: self.epoch,
                now,
                mesh: actor.regional_mesh.as_ref(),
                peers: &actor.provisioned_peers,
                borders: &self.borders,
            },
        };
        for withdrawal in withdrawals {
            withdrawal.send(&router);
        }
        for withdrawal in protected {
            withdrawal.send(&router);
        }
        true
    }

    pub(super) fn independent_media_repair(
        &self,
        actor: &ControllerActor,
        interest: crate::route::fabric::network_repair::MediaInterest,
        now: Instant,
    ) -> Option<crate::route::fabric::network_repair::MediaInterest> {
        let links = actor.links.as_ref()?;
        let source = links.protected_wan_source(interest.parent, now)?;
        let plan = self.protection_plan(actor, source.room, source.publisher.publication, now)?;
        if !plan.demand.source.same_window(source) || plan.demand.source.origin != source.origin {
            return None;
        }
        let parent = self.protected_receivers.alternate_repair_parent(
            links,
            &plan,
            interest.parent,
            interest.position,
            now,
        )?;
        #[cfg(feature = "sim")]
        crate::sim_metrics::wan_repair::selected_independent_repair(
            interest.parent.ingress.association,
            parent.ingress.association,
            interest.position,
        );
        Some(crate::route::fabric::network_repair::MediaInterest { parent, ..interest })
    }

    pub(super) fn media_repair_grants(
        &self,
        peer: NetworkId,
        demand: super::peer_ownership::media_repair::Demand,
        now: Instant,
    ) -> [Option<super::peer_ownership::media_repair::Grant>; 3] {
        [
            self.media_sends.media_repair_grant(peer, demand, now),
            self.media_receives.media_repair_grant(peer, demand, now),
            self.protected_transit.media_repair_grant(peer, demand, now),
        ]
    }

    pub(super) fn target_demand_grants(
        &self,
        peer: NetworkId,
        demand: super::peer_ownership::target_demand::Demand,
        now: Instant,
    ) -> [Option<super::peer_ownership::media_repair::Grant>; 2] {
        [
            self.media_sends.live_media_grant(peer, demand, now),
            self.media_receives.live_media_grant(peer, demand, now),
        ]
    }

    pub(super) fn request_border_media_refresh(
        &self,
        interest: crate::route::fabric::network_repair::MediaInterest,
        now: Instant,
    ) {
        if let Some((_, lane)) = self
            .borders
            .values()
            .find(|border| border.media == Some(interest.parent.ingress.association))
            .and_then(Border::live)
        {
            super::peer_ownership::media_repair::Demand::send_refresh(
                lane, self.epoch, interest, now,
            );
        }
    }

    pub(super) fn catalog_coverage_members(&self) -> impl Iterator<Item = Member> + '_ {
        self.tree
            .catalog_coverage_members()
            .filter(|member| *member != self.local)
    }

    pub(super) fn catalog_members(&self) -> impl Iterator<Item = Member> + '_ {
        self.reads.keys().copied()
    }

    pub(super) fn window(&self, member: Member, order: Order, now: Instant) -> Option<&Window> {
        self.reads.get(&member)?.window(order, now)
    }

    pub(super) fn enqueue(&mut self, member: Member, stream: NonZeroU64, bytes: Box<[u8]>) {
        let input = (Link { member, stream }, bytes);
        if discovery::recognizes(&input.1) {
            if self.discovery.is_some() && self.discovery_inbox.len() < DISCOVERY_INBOX_CAPACITY {
                self.discovery_inbox.push_back(input);
            }
        } else if self.inbox.len() < INBOX_CAPACITY {
            self.inbox.push_back(input);
        }
    }

    fn withdraw_boot(&mut self, member: Member) {
        self.tree.withdraw_boot(member);
        self.reads.remove(&member);
        self.media_targets.retain(|candidate| *candidate != member);
        self.inbox.retain(|(link, _)| link.member != member);
        self.media_client.withdraw_member(member);
        self.media_branches.withdraw_member(member);
    }

    fn admit_boot(&mut self, member: Member) -> bool {
        let previous = self.tree.current_member(member.node);
        if !self.tree.admit_boot(member) {
            return false;
        }
        if let Some(old) = previous
            && old != member
        {
            self.withdraw_boot(old);
        }
        // Unrelated reads and branches keep their original leases. A restarted
        // target no longer has the old boot's catalog row and window namespace.
        self.reads
            .retain(|old, _| old.node != member.node || *old == member);
        if self
            .tree
            .catalog_members()
            .any(|candidate| candidate == member)
        {
            self.reads.entry(member).or_default();
        }
        self.media_targets
            .retain(|old| old.node != member.node || *old == member);
        if self.reads.contains_key(&member)
            && self.tree.media_key(member).is_some()
            && (self.protected_base_stream_bps.is_some()
                || self.tree.region(member) != self.tree.region(self.local))
            && !self.media_targets.contains(&member)
        {
            self.media_targets.push(member);
        }
        self.inbox
            .retain(|(link, _)| link.member.node != member.node || link.member == member);
        true
    }

    fn drive(&mut self, actor: &mut ControllerActor, now: Instant) {
        use super::peer_ownership::media_repair::Demand;
        let mut refreshes = Vec::new();
        let mut target_updates = Vec::new();
        let mut target_budgets = Vec::new();
        for (&member, border) in &mut self.borders {
            border.advance();
            if let Some(lane) = &mut border.lane
                && let Some(stream) = NonZeroU64::new(border.revision)
            {
                for _ in 0..BORDER_QUANTUM {
                    if self.inbox.len() >= INBOX_CAPACITY {
                        break;
                    }
                    let Some(bytes) = lane.try_recv() else { break };
                    if discovery::recognizes(&bytes) {
                        if self.discovery.is_some()
                            && self.discovery_inbox.len() < DISCOVERY_INBOX_CAPACITY
                        {
                            self.discovery_inbox
                                .push_back((Link { member, stream }, bytes));
                        }
                    } else if frames::recognizes(&bytes) {
                        self.inbox.push_back((Link { member, stream }, bytes));
                    } else if let Some((network, demand)) =
                        border.media.zip(Demand::decode_refresh(&bytes))
                    {
                        refreshes.push((network, demand));
                    } else if let Some((network, demand)) = border
                        .media
                        .zip(super::peer_ownership::target_demand::Demand::decode(&bytes))
                    {
                        target_updates.push((member, network, demand));
                    } else if let Some((network, budget)) = border
                        .media
                        .zip(super::peer_ownership::target_demand::Budget::decode(&bytes))
                    {
                        target_budgets.push((network, budget));
                    }
                }
            }
        }
        let refreshes = refreshes
            .into_iter()
            .filter_map(|(network, demand)| {
                let [origin, relay, _] = self.media_repair_grants(network, demand, now);
                let ((Some(grant), None) | (None, Some(grant))) = (origin, relay) else {
                    return None;
                };
                grant.into_refresh(demand)
            })
            .collect();
        let _ = actor.stage_repair_lookups(refreshes);
        if actor
            .receipt_trust
            .as_ref()
            .is_some_and(super::ownership::receipt::Authority::ready)
        {
            for border in self.borders.values() {
                if let Some((network, (_, lane))) = border.media.zip(border.live()) {
                    actor.drive_wan_target_demands(network, lane, self.epoch, now);
                }
            }
            for (network, budget) in target_budgets {
                actor.receive_wan_video_budget(network, budget, now);
            }
            for (member, network, demand) in target_updates {
                let [origin, relay] = self.target_demand_grants(network, demand, now);
                if let (Some(grant), None) | (None, Some(grant)) = (origin, relay) {
                    if let Some((_, lane)) = self.borders.get(&member).and_then(Border::live) {
                        super::peer_ownership::target_demand::reply_with_budget(
                            actor,
                            network,
                            lane,
                            self.epoch,
                            now,
                            demand,
                            grant.expires(),
                        );
                    }
                    grant.apply_target_demand(actor, demand, now);
                }
            }
        }
        if let Some(discovery) = &mut self.discovery {
            let links = Connected {
                epoch: self.epoch,
                now,
                mesh: actor.regional_mesh.as_ref(),
                peers: &actor.provisioned_peers,
                borders: &self.borders,
            };
            let changes = discovery.drive(&self.tree, &links, &mut self.discovery_inbox, now);
            for change in changes {
                match change {
                    discovery::Change::Admit(member) => {
                        self.admit_boot(member);
                    }
                    discovery::Change::Withdraw(member) => self.withdraw_boot(member),
                }
            }
        }
        let router = Router {
            local: self.local,
            tree: &self.tree,
            links: Connected {
                epoch: self.epoch,
                now,
                mesh: actor.regional_mesh.as_ref(),
                peers: &actor.provisioned_peers,
                borders: &self.borders,
            },
        };
        let serve = self
            .tree
            .catalog_members()
            .any(|member| member == self.local);
        let wall_now = SystemTime::now();
        self.media_branches.expire(now, wall_now);
        while let Some((from, bytes)) = self.inbox.pop_front() {
            match self.forwarding.receive(&router, from, &bytes, now) {
                Some(Arrival::Request(ticket, request)) if serve => {
                    if let Some(MessageOnStream::Get(id, get)) = neighbor::decode(request)
                        && let Some(reply) = get.serve_routed(id, actor)
                    {
                        router.reply(ticket, &reply, now);
                    } else if self.media_key.is_some()
                        && let Some(get) = media_read::Get::decode(request)
                    {
                        self.media_owner.enqueue(ticket, get);
                    }
                }
                Some(Arrival::Reply(owner, reply)) => {
                    if let Some(MessageOnStream::Reply(id, result)) = neighbor::decode(reply)
                        && let Some(reads) = self.reads.get_mut(&owner)
                    {
                        let orders = orders(actor, owner);
                        if let Some((watch, result)) = reads.receive(id, result, now, &orders) {
                            actor.catalog.receive_remote(watch, result);
                        }
                    } else if let Some(reply) = media_read::Reply::decode(reply) {
                        self.media_client
                            .receive(actor, &self.tree, owner, reply, now, wall_now);
                    }
                }
                Some(Arrival::Branch(request)) => {
                    if let Some(edge) = router.links.media_edge(from)
                        && let Some(lease) = self
                            .media_branches
                            .receive(&self.tree, self.local, edge, request, now, wall_now)
                        && lease.protection.is_none()
                    {
                        let label = lease.ack.window.ssrc();
                        if self
                            .media_receives
                            .transit_label_retired(edge.network, label)
                            && let Some(frame) = frames::Frame::BranchRetired(label).encode()
                        {
                            router.links.send(from, frame, lease.expires);
                        }
                    }
                }
                Some(Arrival::BranchRetired(label)) => {
                    if let Some(edge) = router.links.media_edge(from)
                        && let Some(links) = actor.links.as_mut()
                    {
                        self.media_receives.retire_remote_label(links, edge, label);
                    }
                }
                _ => {}
            }
        }
        if let Some(key) = &self.media_key {
            self.media_owner.flush(actor, &router, key, now, wall_now);
        }
        for (&owner, reads) in &mut self.reads {
            let orders = orders(actor, owner);
            reads.prune(&orders, now, |watch| {
                actor
                    .catalog
                    .notify(watch, super::catalog::CatalogChange::Changed);
            });
            reads.send(&orders, now, |id, order, after, revision| {
                let Some(discover) = actor.catalog.discover_for(actor, order.demand.watch) else {
                    return false;
                };
                let get = Get {
                    project: order.demand.watch.scope.room.project,
                    discover,
                    interest: ROUTED.interest,
                    after,
                    revision,
                };
                get.encode(id).is_some_and(|payload| {
                    let [attempt, ..] = id.get().to_le_bytes();
                    self.forwarding
                        .request(
                            &router,
                            owner,
                            attempt,
                            &payload,
                            now,
                            forwarding::ROUTED_READ_TTL,
                        )
                        .is_some()
                })
            });
        }
        self.media_client.step(
            actor,
            &router,
            &mut self.forwarding,
            &self.media_targets,
            now,
            wall_now,
        );
        let protected_plans = self
            .media_branches
            .protected()
            .filter_map(|lease| media_protection::TransitPlan::new(&router, lease))
            .collect();
        let receiver_plans = self.protected_base_stream_bps.map_or_else(
            media_protection::ReceiverPlans::default,
            |wire_bps| {
                media_protection::receiver_plans(
                    &router,
                    self.media_client.demands(),
                    wire_bps,
                    now,
                )
            },
        );
        let plans = media_ingress::plan(
            &router,
            self.media_client.demands(),
            self.media_branches.iter(),
            |target| self.media_client.attempt_for(target),
            self.protected_base_stream_bps.is_some(),
        );
        self.media_owner.stage(actor, now, wall_now);
        let updates = self
            .media_receives
            .step(actor, plans, &self.protected_receivers, now);
        let protected_updates = actor
            .links
            .as_mut()
            .map(|links| self.protected_transit.step(links, protected_plans, now))
            .unwrap_or_default();
        let receiver_updates = self.protected_receivers.step(
            actor,
            &self.media_receives,
            self.local,
            receiver_plans,
            now,
        );
        self.media_sends
            .step(actor, self.local, &self.media_branches, now, wall_now);
        let router = Router {
            local: self.local,
            tree: &self.tree,
            links: Connected {
                epoch: self.epoch,
                now,
                mesh: actor.regional_mesh.as_ref(),
                peers: &actor.provisioned_peers,
                borders: &self.borders,
            },
        };
        for update in updates {
            update.send(&router);
        }
        for update in protected_updates.into_iter().chain(receiver_updates) {
            update.send(&router);
        }
    }
}

fn orders(actor: &ControllerActor, owner: Member) -> Vec<Order> {
    actor
        .catalog
        .demands(actor, (owner.node, owner.generation))
        .map(|demand| Order {
            demand,
            declaration: ROUTED,
        })
        .collect()
}

impl ControllerActor {
    pub(crate) fn set_region_tree(
        &mut self,
        local: Member,
        tree: RegionTree,
        epoch: Instant,
        protected_base_stream_bps: Option<NonZeroU64>,
    ) {
        let reads = tree
            .catalog_members()
            .filter(|&member| member != local)
            .map(|member| (member, Reads::default()))
            .collect();
        let media_targets = tree
            .catalog_members()
            .filter(|&member| {
                member != local
                    && tree.media_key(member).is_some()
                    && (protected_base_stream_bps.is_some()
                        || tree.region(member) != tree.region(local))
            })
            .collect();
        self.regional_routing = Some(Routing {
            local,
            tree,
            epoch,
            reads,
            borders: BTreeMap::new(),
            enrolled_borders: BTreeMap::new(),
            admitted_boots: None,
            discovery: None,
            discovery_inbox: VecDeque::new(),
            inbox: VecDeque::new(),
            forwarding: Forwarding::default(),
            media_key: None,
            media_owner: media_read::OwnerReads::default(),
            media_targets,
            media_client: media_client::Client::default(),
            media_branches: media_branch::Leases::default(),
            media_sends: media_branch::OriginSends::default(),
            media_receives: media_ingress::Receives::default(),
            protected_transit: media_protection::Transit::default(),
            protected_base_stream_bps,
            protected_receivers: media_protection::Receivers::default(),
        });
    }

    pub(crate) fn set_regional_media_key(&mut self, key: &ApiSigningKey) -> anyhow::Result<()> {
        if let Some(routing) = self.regional_routing.as_mut()
            && let Some(pin) = routing.tree.media_key(routing.local)
        {
            anyhow::ensure!(
                key.public_key() == pin,
                "regional media verifier does not match the local identity key"
            );
            routing.media_key = Some(key.clone());
        }
        Ok(())
    }

    pub(crate) fn add_regional_border(&mut self, member: Member, port: Port) -> anyhow::Result<()> {
        let routing = self
            .regional_routing
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("border requires a region tree"))?;
        anyhow::ensure!(
            routing
                .tree
                .adjacency(routing.local, member)
                .is_some_and(|scope| scope.is_border() && scope.roles == port.roles),
            "invalid regional border port"
        );
        match routing.borders.entry(member) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(Border {
                    port: Some(port),
                    ..Border::default()
                });
                Ok(())
            }
            std::collections::btree_map::Entry::Occupied(_) => {
                anyhow::bail!("duplicate regional border port")
            }
        }
    }

    pub(crate) fn attach_regional_border_media(
        &mut self,
        remote: Member,
        peer: NetworkPeer,
        host: crate::link::wan_host::Provisioner,
    ) -> anyhow::Result<()> {
        let routing = self
            .regional_routing
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("border media requires a region tree"))?;
        let border = routing
            .borders
            .get_mut(&remote)
            .ok_or_else(|| anyhow::anyhow!("border media requires a registered control port"))?;
        anyhow::ensure!(border.media.is_none(), "duplicate border media association");
        let links = self
            .links
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("border media requires a link controller"))?;
        let network = links
            .reserve_wan_peer()
            .map_err(|error| anyhow::anyhow!("reserving border media association: {error:?}"))?;
        let commands = host
            .open_media(peer, network, routing.local.generation)
            .map_err(|error| anyhow::anyhow!("opening border media path: {error:?}"))?;
        links
            .attach_wan_host(commands, crate::control::link::wan::Carriage::Border)
            .map_err(|error| anyhow::anyhow!("installing border media path: {error:?}"))?;
        links.identify_peer(network, remote.node);
        border.media = Some(network);
        Ok(())
    }

    pub(crate) fn set_regional_discovery(
        &mut self,
        epoch: [u8; 16],
        key: ApiSigningKey,
        direct: BTreeSet<NodeId>,
        clock: Snapshot,
        clocks: pulsebeam_queue::Receiver<Snapshot>,
        port: DiscoveryPort,
    ) -> anyhow::Result<()> {
        let routing = self
            .regional_routing
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("boot discovery requires a region tree"))?;
        anyhow::ensure!(
            routing.tree.discovery_key(routing.local) == Some(key.public_key()),
            "regional boot verifier does not match local signing key"
        );
        anyhow::ensure!(
            routing.discovery.is_none(),
            "regional boot discovery is already wired"
        );
        let scope = boot_announcement::Scope {
            epoch,
            roster: *routing.tree.fingerprint(),
        };
        routing.discovery = Some(discovery::Discovery::new(
            routing.local,
            scope,
            key,
            direct,
            clock,
            clocks,
            port,
        ));
        Ok(())
    }

    pub(crate) fn set_regional_boot_admissions(
        &mut self,
        admitted: mpsc::Receiver<AdmittedBoot>,
    ) -> anyhow::Result<()> {
        let routing = self
            .regional_routing
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("boot admission requires a region tree"))?;
        anyhow::ensure!(
            routing.admitted_boots.is_none(),
            "regional boot admission is already wired"
        );
        routing.admitted_boots = Some(admitted);
        Ok(())
    }

    pub(crate) fn add_enrolled_border(
        &mut self,
        peer: NodeId,
        transport: MediaCredentials,
        host: crate::link::wan_host::Provisioner,
    ) -> anyhow::Result<()> {
        let routing = self
            .regional_routing
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("border enrollment requires a region tree"))?;
        anyhow::ensure!(
            routing
                .tree
                .enrolled_adjacency(routing.local.node, peer)
                .is_some_and(|scope| scope.is_border()),
            "invalid enrolled border slot"
        );
        match routing.enrolled_borders.entry(peer) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(EnrolledBorder {
                    transport,
                    host,
                    current: None,
                });
                Ok(())
            }
            std::collections::btree_map::Entry::Occupied(_) => {
                anyhow::bail!("duplicate enrolled border slot")
            }
        }
    }

    fn admit_border_boot(&mut self, boot: BootSession) -> anyhow::Result<()> {
        let routing = self
            .regional_routing
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("border admission requires a region tree"))?;
        let slot = routing
            .enrolled_borders
            .get_mut(&boot.peer.node)
            .ok_or_else(|| anyhow::anyhow!("admitted boot has no border slot"))?;
        let old_network = match slot.current.replace(boot.peer) {
            Some(old) if old != boot.peer => {
                routing.borders.remove(&old).and_then(|border| border.media)
            }
            _ => None,
        };
        let transport = slot.transport.pin(boot.media_leaf);
        let host = slot.host.clone();
        let needs_media = routing
            .borders
            .entry(boot.peer)
            .or_default()
            .media
            .is_none();
        if let Some(network) = old_network {
            self.close_link_peer(network);
        }
        if needs_media {
            self.attach_regional_border_media(boot.peer, transport, host)?;
        }
        if let Some(border) = self
            .regional_routing
            .as_mut()
            .and_then(|routing| routing.borders.get_mut(&boot.peer))
        {
            border.install_lane(boot.control);
        }
        Ok(())
    }

    pub(super) fn drive_regional_boots(&mut self, now: Instant) {
        for _ in 0..ADMISSION_QUANTUM {
            let Some(routing) = &mut self.regional_routing else {
                break;
            };
            let Some(boot) = routing
                .admitted_boots
                .as_mut()
                .and_then(|rx| rx.try_recv().ok())
            else {
                break;
            };
            let peer = boot.peer();
            if !routing.admit_boot(peer) {
                continue;
            }
            let installed = if routing.enrolled_borders.contains_key(&peer.node) {
                self.admit_border_boot(boot.into_session())
            } else {
                self.admit_ownership_boot(boot, now)
            };
            if let Err(error) = installed {
                tracing::warn!(%error, "could not install admitted regional boot");
            }
        }
    }

    pub(super) fn regional_selector_empty_confirmed(
        &self,
        owner: super::native_service::MediaInterestId,
        revision: u64,
        now: Instant,
    ) -> bool {
        self.regional_routing.as_ref().is_none_or(|routing| {
            routing.media_targets.is_empty()
                || routing.media_client.empty_confirmed(owner, revision, now)
        })
    }

    pub(super) fn drive_regional_routing(&mut self, now: Instant) {
        if let Some(mut routing) = self.regional_routing.take() {
            routing.drive(self, now);
            let changes = routing.media_client.take_coverage_changes();
            self.regional_routing = Some(routing);
            for owner in changes {
                self.observe_regional_selector_coverage(owner);
            }
        }
    }
}
