//! Bounded catalog and media owner reads over the region tree. Routing membership and border
//! sessions are independent of publisher issuers and ownership observations.

use std::{
    collections::{BTreeMap, VecDeque},
    num::NonZeroU64,
    time::SystemTime,
};

use pulsebeam_core::auth::ApiSigningKey;
use pulsebeam_link::{LinkTime, NetworkId, NetworkPeer, PeerControl};
use pulsebeam_runtime::time::Instant;

use crate::cluster::{
    RegionTree,
    mesh::{Exchange, Member, Port},
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

mod forwarding;
mod frames;
mod media_branch;
mod media_client;
mod media_ingress;
mod media_permit;
mod media_read;

use forwarding::{Arrival, Forwarding, Link, Links, Router};
pub(super) use frames::recognizes;

const INBOX_CAPACITY: usize = 64;
const BORDER_QUANTUM: usize = 4;

// A routed read is anchored to the target member and the owner's row revision,
// not to a declaration or stream belonging to a replaceable next hop.
pub(super) const ROUTED: DeclarationId = DeclarationId {
    stream: NonZeroU64::MIN,
    interest: NonZeroU64::MIN,
    changed_at: NonZeroU64::MIN,
};

struct Border {
    port: Port,
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
        if let Some(next) = self.revision.checked_add(1)
            && let Ok(lane) = self.port.sessions.try_recv()
            && lane.is_ready()
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
    mesh: Option<&'a Exchange>,
    peers: &'a [Peer],
    borders: &'a BTreeMap<Member, Border>,
}

impl Connected<'_> {
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
        self.mesh?.path(destination)?.first().copied()
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
}

impl Routing {
    pub(super) fn catalog_members(&self) -> impl Iterator<Item = Member> + '_ {
        self.reads.keys().copied()
    }

    pub(super) fn window(&self, member: Member, order: Order, now: Instant) -> Option<&Window> {
        self.reads.get(&member)?.window(order, now)
    }

    pub(super) fn enqueue(&mut self, member: Member, stream: NonZeroU64, bytes: Box<[u8]>) {
        if self.inbox.len() < INBOX_CAPACITY {
            self.inbox.push_back((Link { member, stream }, bytes));
        }
    }

    fn drive(&mut self, actor: &mut ControllerActor, now: Instant) {
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
                    if recognizes(&bytes) {
                        self.inbox.push_back((Link { member, stream }, bytes));
                    }
                }
            }
        }
        let router = Router {
            local: self.local,
            tree: &self.tree,
            links: Connected {
                epoch: self.epoch,
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
                    if let Some(edge) = router.links.media_edge(from) {
                        self.media_branches
                            .receive(&self.tree, self.local, edge, request, now, wall_now);
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
                        .request(&router, owner, attempt, &payload, now)
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
        let plans = media_ingress::plan(
            &router,
            self.media_client.demands(),
            self.media_branches.iter(),
            |target| self.media_client.attempt_for(target),
        );
        self.media_owner.stage(actor, now);
        let updates = self.media_receives.step(actor, plans, now);
        self.media_sends
            .step(actor, self.local, &self.media_branches, now, wall_now);
        let router = Router {
            local: self.local,
            tree: &self.tree,
            links: Connected {
                epoch: self.epoch,
                mesh: actor.regional_mesh.as_ref(),
                peers: &actor.provisioned_peers,
                borders: &self.borders,
            },
        };
        for update in updates {
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
    pub(crate) fn set_region_tree(&mut self, local: Member, tree: RegionTree, epoch: Instant) {
        let reads = tree
            .catalog_members()
            .filter(|&member| member != local)
            .map(|member| (member, Reads::default()))
            .collect();
        let media_targets = tree
            .catalog_members()
            .filter(|&member| {
                tree.region(member) != tree.region(local) && tree.media_key(member).is_some()
            })
            .collect();
        self.regional_routing = Some(Routing {
            local,
            tree,
            epoch,
            reads,
            borders: BTreeMap::new(),
            inbox: VecDeque::new(),
            forwarding: Forwarding::default(),
            media_key: None,
            media_owner: media_read::OwnerReads::default(),
            media_targets,
            media_client: media_client::Client::default(),
            media_branches: media_branch::Leases::default(),
            media_sends: media_branch::OriginSends::default(),
            media_receives: media_ingress::Receives::default(),
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
                    port,
                    lane: None,
                    revision: 0,
                    media: None,
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

    pub(super) fn drive_regional_routing(&mut self, now: Instant) {
        if let Some(mut routing) = self.regional_routing.take() {
            routing.drive(self, now);
            self.regional_routing = Some(routing);
        }
    }
}
