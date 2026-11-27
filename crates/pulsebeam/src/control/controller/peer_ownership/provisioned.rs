//! Operator-provisioned point-to-point ownership observation over Link.
//! No address, certificate, or incarnation is learned from membership gossip.

use std::{
    collections::HashSet,
    num::{NonZeroU16, NonZeroU64},
    time::Duration,
};

use pulsebeam_core::identity::NodeId;
use pulsebeam_link::{LinkTime, NetworkId, NetworkPeer, PeerControl};
use pulsebeam_runtime::{
    sync::{mpsc, oneshot},
    time::Instant,
};

use super::super::publisher_handoffs::Frame as HandoffFrame;
use super::{
    ControllerActor, Session, fabric_data, media_repair,
    ownership::{
        receipt::{Authority, Issuer, epoch::Trust, standby::wire::Frame as StandbyFrame},
        wire,
    },
    relay::{self, interest, wan_source::WanSourceWindow, wan_transit},
    transfer::{self, Frame},
};

use crate::{
    cluster::{
        ProvisionedPeer,
        declarations::{Declaration, Exchange, Finalized, Kind, Role},
        mesh,
    },
    control::{
        controller::catalog::{
            CatalogChange,
            neighbor::{self, client},
            responders::DeclarationId,
        },
        link::wan::Carriage,
    },
    entity::ProjectRoomKey,
    route::fabric::{
        network_repair::{Interest, MAX_REPAIR_LIFETIME},
        relay::LocatedRepair,
    },
    shard_update::PublisherAnchor,
};

const REFRESH: Duration = Duration::from_secs(3);
const REOPEN: Duration = Duration::from_secs(1);
const LIVENESS: Duration = Duration::from_secs(15);
const FRAME_TTL: Duration = Duration::from_millis(450);
const WAN_REFRESH_MIN_INTERVAL: Duration = Duration::from_millis(450);
const RECEIVE_BUDGET: usize = 16;
const EXPECTATION_BUDGET: usize = 32;
pub(super) const REPAIR_LOOKUP_BUDGET: usize = 32;

pub(in crate::control) struct Peer {
    remote: (NodeId, NonZeroU64),
    transport: Transport,
    relay_from: Option<NodeId>,
    association: Option<Association>,
    next_step: Instant,
    last_revision: u64,
    last_stream: u64,
}

enum Transport {
    #[cfg(test)]
    CatalogFixture,
    Wan(WanHost),
    Regional {
        port: mesh::Port,
        media: WanHost,
    },
}

struct WanHost {
    peer: NetworkPeer,
    generation: NonZeroU64,
    host: crate::link::wan_host::Provisioner,
}

impl Transport {
    fn declarations(&self) -> Option<Exchange> {
        match self {
            Self::Regional { port, .. } => Some(Exchange::new(port.roles.0, port.roles.1)),
            _ => None,
        }
    }

    fn open(
        &mut self,
        links: &mut crate::control::link::Links,
        remote: NodeId,
    ) -> Result<(NetworkId, PeerControl), pulsebeam_link::LinkError> {
        use pulsebeam_link::LinkError;
        match self {
            #[cfg(test)]
            Self::CatalogFixture => Err(LinkError::Closed),
            Self::Wan(media) => {
                let id = links.reserve_wan_peer()?;
                let (commands, lane) = media.host.open(
                    media.peer.clone(),
                    id,
                    media.generation,
                    links.clock_origin(),
                )?;
                links
                    .attach_wan_host(commands, Carriage::Ownership)
                    .map_err(|_| LinkError::Capacity)?;
                links.identify_peer(id, remote);
                Ok((id, lane))
            }
            Self::Regional { port, media } => {
                let lane = port.sessions.try_recv().map_err(|_| LinkError::Closed)?;
                if lane.is_closed() {
                    return Err(LinkError::Closed);
                }
                let id = links.reserve_wan_peer()?;
                let commands = media
                    .host
                    .open_media(media.peer.clone(), id, media.generation)?;
                links
                    .attach_wan_host(commands, Carriage::Ownership)
                    .map_err(|_| LinkError::Capacity)?;
                links.identify_peer(id, remote);
                Ok((id, lane))
            }
        }
    }
}

struct PendingExpectation {
    frame: fabric_data::Frame,
    expires: Instant,
    receipts: Vec<oneshot::Receiver<bool>>,
    failed: bool,
}

type LocationReceipt = oneshot::Receiver<Option<LocatedRepair>>;

#[derive(Default)]
pub(in crate::control) struct RepairLookups {
    pending: Vec<PendingRepairLookup>,
    paused_until: Option<Instant>,
}

struct PendingRepairLookup {
    frame: fabric_data::RepairFrame,
    expires: Instant,
    phase: RepairLookupPhase,
}

enum RepairLookupPhase {
    Resolving {
        receipts: Vec<LocationReceipt>,
        found: Option<LocatedRepair>,
        conflicting: bool,
    },
    Reply {
        found: Option<LocatedRepair>,
        announced: bool,
        repeat: bool,
    },
    Consumed(Option<pulsebeam_link::BindingId>),
    Cancelled,
    Silent,
}

#[cfg(test)]
mod tests;

impl RepairLookups {
    fn entry(
        &mut self,
        scope: fabric_data::Scope,
        branch: NonZeroU64,
        request: NonZeroU64,
    ) -> Option<&mut PendingRepairLookup> {
        self.pending.iter_mut().find(|entry| {
            entry.frame.scope == scope
                && entry.frame.branch == branch
                && entry.frame.request.id == request
        })
    }

    fn outside_reorder_window(&self, frame: fabric_data::RepairFrame) -> bool {
        // Independent fragment lookups can arrive out of order on the DTLS lane.
        // Keep exact replay state in pending and allow reordering within one unit's burst.
        let width = u64::from(pulsebeam_proto::data_record::MAX_FRAGMENTS);
        self.pending.iter().any(|entry| {
            entry.frame.scope == frame.scope
                && entry.frame.branch == frame.branch
                && entry
                    .frame
                    .request
                    .id
                    .get()
                    .saturating_sub(frame.request.id.get())
                    >= width
        })
    }

    pub(in crate::control) fn lookup(
        &mut self,
        frame: fabric_data::RepairFrame,
        now: Instant,
        stage: impl FnOnce() -> Vec<LocationReceipt>,
    ) {
        if let Some(entry) = self.entry(frame.scope, frame.branch, frame.request.id) {
            if entry.frame == frame
                && now < entry.expires
                && let RepairLookupPhase::Reply { repeat, .. } = &mut entry.phase
            {
                *repeat = true;
            }
            return;
        }
        if self.paused_until.is_some_and(|limit| now < limit)
            || self.pending.len() >= REPAIR_LOOKUP_BUDGET
            || self.outside_reorder_window(frame)
        {
            return;
        }
        let Some(expires) = now.checked_add(MAX_REPAIR_LIFETIME) else {
            return;
        };
        let receipts = stage();
        if !receipts.is_empty() {
            self.pending.push(PendingRepairLookup {
                frame,
                expires,
                phase: RepairLookupPhase::Resolving {
                    receipts,
                    found: None,
                    conflicting: false,
                },
            });
        }
    }

    pub(in crate::control) fn flush(
        &mut self,
        now: Instant,
        mut send: impl FnMut(fabric_data::RepairFrame) -> bool,
    ) {
        self.pending.retain_mut(|entry| {
            if now >= entry.expires {
                return false;
            }
            let frame = entry.frame;
            entry.flush(now, |action| {
                send(fabric_data::RepairFrame { action, ..frame })
            });
            true
        });
    }

    pub(in crate::control) fn cancel(
        &mut self,
        frame: fabric_data::RepairFrame,
        now: Instant,
    ) -> Option<pulsebeam_link::BindingId> {
        let expires = now.checked_add(MAX_REPAIR_LIFETIME)?;
        if let Some(entry) = self.entry(frame.scope, frame.branch, frame.request.id) {
            if entry.frame.request != frame.request {
                return None;
            }
            entry.expires = entry.expires.max(expires);
            return match std::mem::replace(&mut entry.phase, RepairLookupPhase::Cancelled) {
                RepairLookupPhase::Consumed(binding) => binding,
                _ => None,
            };
        }
        if self.pending.len() >= REPAIR_LOOKUP_BUDGET {
            // An unrecorded cancel cannot be followed by a new lookup during its lease.
            self.paused_until = Some(expires);
        } else {
            self.pending.push(PendingRepairLookup {
                frame,
                expires,
                phase: RepairLookupPhase::Cancelled,
            });
        }
        None
    }

    fn ready_matching(
        &mut self,
        ready: fabric_data::WanRepairReady,
        now: Instant,
        matches_location: impl FnOnce(&LocatedRepair) -> bool,
        handoff: impl FnOnce(LocatedRepair) -> Option<pulsebeam_link::BindingId>,
    ) {
        let Some(entry) = self
            .entry(ready.scope, ready.branch, ready.request.id)
            .filter(|entry| entry.frame.request == ready.request && now < entry.expires)
        else {
            return;
        };
        let RepairLookupPhase::Reply {
            found,
            announced: true,
            ..
        } = &mut entry.phase
        else {
            return;
        };
        if found.as_ref().is_some_and(|found| {
            now < found.expires && ready.sequence == found.sequence && matches_location(found)
        }) && let Some(mut located) = found.take()
        {
            located.expires = located.expires.min(entry.expires);
            entry.phase = RepairLookupPhase::Consumed(handoff(located));
        }
    }
}

impl PendingRepairLookup {
    fn flush(&mut self, now: Instant, send: impl FnOnce(fabric_data::RepairAction) -> bool) {
        use fabric_data::RepairAction;
        if let RepairLookupPhase::Resolving {
            receipts,
            found,
            conflicting,
        } = &mut self.phase
        {
            receipts.retain_mut(|receipt| match receipt.try_recv() {
                Ok(Some(location)) => {
                    *conflicting |= found.replace(location).is_some();
                    false
                }
                Ok(None) => false,
                Err(oneshot::error::TryRecvError::Closed) => {
                    *conflicting = true;
                    false
                }
                Err(oneshot::error::TryRecvError::Empty) => true,
            });
            if !receipts.is_empty() {
                return;
            }
            self.phase = if *conflicting {
                RepairLookupPhase::Silent
            } else {
                RepairLookupPhase::Reply {
                    found: found.take(),
                    announced: false,
                    repeat: true,
                }
            };
        }
        let RepairLookupPhase::Reply {
            found,
            announced,
            repeat,
        } = &mut self.phase
        else {
            return;
        };
        if !*repeat {
            return;
        }
        if found.as_ref().is_some_and(|located| now >= located.expires) {
            self.phase = RepairLookupPhase::Silent;
            return;
        }
        let action =
            found
                .as_ref()
                .map_or(RepairAction::Unavailable, |located| RepairAction::Located {
                    sequence: located.sequence,
                });
        if send(action) {
            *repeat = false;
            *announced = matches!(action, RepairAction::Located { .. });
        }
    }
}

struct RepairInterest {
    interest: Interest,
    state: RepairInterestState,
}

enum RepairInterestState {
    Lookup { sent: bool },
    WanRepair(crate::route::fabric::network_repair::RepairChild),
    Cancelled,
}

impl RepairInterest {
    fn accepts_lookup_reply(&self, frame: fabric_data::RepairFrame, now: Instant) -> bool {
        matches!(self.state, RepairInterestState::Lookup { sent: true })
            && now < self.interest.expires
            && self.frame(frame.action) == frame
    }

    fn frame(&self, action: fabric_data::RepairAction) -> fabric_data::RepairFrame {
        fabric_data::RepairFrame {
            scope: fabric_data::Scope {
                key: self.interest.parent.ingress.key,
                source: self.interest.parent.incarnation,
            },
            branch: self.interest.branch,
            request: self.interest.request,
            action,
        }
    }
}

type WanSample = (
    (crate::entity::ProjectRoomKey, PublisherAnchor),
    oneshot::Receiver<Option<crate::shard_update::PublisherPosition>>,
);

struct PendingImage {
    revision: NonZeroU64,
    expires: Instant,
    wan_samples: Vec<WanSample>,
    wan_sources: Vec<WanSourceWindow>,
}

struct Association {
    network: NetworkId,
    lane: PeerControl,
    declarations: Option<Exchange>,
    mesh_outbound: Option<mesh::Outbound>,
    stream: NonZeroU64,
    awaiting_wan_refresh: bool,
    catalog_reads: client::Reads,
    epoch: Instant,
    session: Option<Session>,
    reader: transfer::Reader,
    sender: transfer::Sender,
    pending_image: Option<PendingImage>,
    standby_cursor: usize,
    data_expectations: Vec<PendingExpectation>,
    repair_lookups: RepairLookups,
    repair_interests: Vec<RepairInterest>,
    local_wan_receipts: Box<[super::relay::fabric::WanLiveRouteAck]>,
    local_wan_sources: Box<[WanSourceWindow]>,
    offer_cut: u64,
    wan_publishers: super::relay::wan_publisher::PublisherSends,
    wan_repairs: super::relay::wan_repair::RepairSends,
    media_repairs: media_repair::RepairRequests,
    media_receives: media_repair::RepairReceives,
    wan_transit: wan_transit::Sends,
    transit_advertisements: Vec<wan_transit::Advertisement>,
    exports: Vec<wan_transit::Export>,
    remote_wan: Option<(Instant, wire::WanOwnership)>,
    interest_after: Option<relay::Key>,
    wan_refresh: WanRefresh,
    refresh: Instant,
    live_until: Instant,
}

struct WanRefresh {
    announce: bool,
    announce_after: Instant,
    pull: bool,
    pull_after: Instant,
    retired: Vec<[u8; 4]>,
}

impl WanRefresh {
    fn new(now: Instant) -> Self {
        Self {
            announce: false,
            announce_after: now,
            pull: false,
            pull_after: now,
            retired: Vec::new(),
        }
    }
}

impl Peer {
    fn new(
        remote: (NodeId, NonZeroU64),
        transport: Transport,
        relay_from: Option<NodeId>,
        now: Instant,
    ) -> Self {
        Self {
            remote,
            transport,
            relay_from,
            association: None,
            next_step: now,
            last_revision: 0,
            last_stream: 0,
        }
    }

    pub(in crate::control) fn catalog_responder(&self) -> (NodeId, NonZeroU64) {
        self.remote
    }

    pub(in crate::control::controller) fn catalog_declaration(
        &self,
        room: ProjectRoomKey,
        now: Instant,
    ) -> Option<(DeclarationId, Finalized)> {
        self.association.as_ref()?.catalog_declaration(room, now)
    }

    pub(in crate::control::controller) fn catalog_window(
        &self,
        order: client::Order,
        now: Instant,
    ) -> Option<&client::Window> {
        self.association.as_ref()?.catalog_reads.window(order, now)
    }

    pub(in crate::control) fn deadline(&self) -> Instant {
        self.next_step
    }

    pub(in crate::control) fn matches_network(&self, peer: NetworkId) -> bool {
        self.association
            .as_ref()
            .is_some_and(|assoc| assoc.network == peer)
    }

    pub(in crate::control) fn allows_wan_data_expectation(
        &self,
        expectation: &crate::route::fabric::PublisherExpectation,
        now: Instant,
    ) -> bool {
        self.association.as_ref().is_some_and(|association| {
            association.lane.is_ready()
                && association
                    .wan_publishers
                    .allows_data_expectation(expectation, now)
        })
    }

    pub(in crate::control) fn send_publisher_data_expectation(
        &self,
        expectation: crate::route::fabric::PublisherExpectation,
        now: Instant,
    ) {
        self.send_data_expectation(
            expectation.route.next,
            expectation.route.publisher.incarnation,
            expectation.unit,
            expectation.expires.min(expectation.route.tx.expires()),
            now,
        );
    }

    fn send_data_expectation(
        &self,
        next: crate::route::fabric::LiveNextHop,
        source: NonZeroU64,
        unit: crate::route::fabric::local::DataExpectation,
        expires: Instant,
        now: Instant,
    ) {
        let Some(association) = self
            .association
            .as_ref()
            .filter(|association| association.network == next.peer && association.lane.is_ready())
        else {
            return;
        };
        let milliseconds = expires
            .saturating_duration_since(now)
            .as_millis()
            .min(2_000);
        let Ok(lease_ms) = u16::try_from(milliseconds) else {
            return;
        };
        let Some(expires_at) = u64::try_from(
            expires
                .saturating_duration_since(association.epoch)
                .as_nanos(),
        )
        .ok()
        .map(LinkTime) else {
            return;
        };
        let frame = fabric_data::Frame {
            scope: fabric_data::Scope {
                key: next.key,
                source,
            },
            unit,
            action: fabric_data::Action::Expect { lease_ms },
        };
        if let Some(bytes) = frame.encode() {
            let _ = association.lane.try_send(bytes, expires_at);
        }
    }

    pub(in crate::control) fn request_media_repair(
        &mut self,
        interest: crate::route::fabric::network_repair::MediaInterest,
        now: Instant,
    ) {
        if let Some(assoc) = &mut self.association
            && assoc.network == interest.parent.ingress.association
            && assoc.lane.is_ready()
            && assoc.media_receives.enqueue(interest, now)
        {
            self.next_step = self.next_step.min(now);
        }
    }

    pub(in crate::control) fn request_repair(&mut self, interest: Interest, now: Instant) {
        let Some(association) = self.association.as_mut().filter(|association| {
            association.network == interest.parent.ingress.association
                && interest.parent.window.is_some()
                && association.lane.is_ready()
        }) else {
            return;
        };
        let pending = RepairInterest {
            interest,
            state: RepairInterestState::Lookup { sent: false },
        };
        let lookup = pending.frame(fabric_data::RepairAction::Lookup);
        if now >= interest.expires
            || now
                .checked_add(MAX_REPAIR_LIFETIME)
                .is_none_or(|limit| interest.expires > limit)
            || association.repair_interests.len() >= REPAIR_LOOKUP_BUDGET
            || association
                .repair_interests
                .iter()
                .any(|pending| pending.frame(lookup.action) == lookup)
        {
            return;
        }
        association.repair_interests.push(pending);
        self.next_step = self.next_step.min(now);
    }

    pub(in crate::control) fn cancel_repair(&mut self, interest: Interest, now: Instant) {
        if let Some(association) = &mut self.association
            && let Some(pending) = association
                .repair_interests
                .iter_mut()
                .find(|pending| pending.interest == interest)
        {
            pending.state = RepairInterestState::Cancelled;
            self.next_step = self.next_step.min(now);
        }
    }

    fn mesh_member(&self) -> mesh::Member {
        mesh::Member {
            node: self.remote.0,
            generation: self.remote.1,
        }
    }

    pub(in crate::control::controller) fn routed_control(
        &self,
        member: mesh::Member,
    ) -> Option<(NonZeroU64, &PeerControl)> {
        (self.mesh_member() == member).then_some(())?;
        let association = self.association.as_ref()?;
        (association.mesh_outbound.is_some() && association.lane.is_ready())
            .then_some((association.stream, &association.lane))
    }

    pub(in crate::control::controller) fn routed_media(
        &self,
        member: mesh::Member,
    ) -> Option<NetworkId> {
        (self.mesh_member() == member).then_some(())?;
        let association = self.association.as_ref()?;
        association
            .mesh_outbound
            .is_some()
            .then_some(association.network)
    }

    fn live_mesh_link(&self, now: Instant) -> Option<(mesh::Member, NonZeroU16)> {
        let Transport::Regional { port, .. } = &self.transport else {
            return None;
        };
        let association = self.association.as_ref()?;
        (association.session.is_some() && association.has_live_view(now))
            .then(|| (self.mesh_member(), port.cost.get()))
    }

    fn step(&mut self, actor: &mut ControllerActor, now: Instant) {
        if self.association.as_ref().is_some_and(|assoc| {
            now >= assoc.live_until || (assoc.mesh_outbound.is_none() && assoc.lane.is_closed())
        }) {
            // An expired authority view still invalidates the routes it established.
            if let Some(assoc) = self.association.take() {
                actor.close_link_peer(assoc.network);
                if let Some(session) = assoc.session {
                    let _ = actor.close_ownership_peer(session);
                }
            }
            self.next_step = now.checked_add(REOPEN).unwrap_or(now);
        }
        if now < self.next_step {
            return;
        }
        self.next_step = now.checked_add(transfer::RETRY).unwrap_or(now);
        let member = self.mesh_member();
        if let Some(assoc) = &mut self.association {
            if assoc.lane.is_closed()
                && let Transport::Regional { port, .. } = &mut self.transport
                && let Some(stream) = self.last_stream.checked_add(1).and_then(NonZeroU64::new)
                && let Ok(lane) = port.sessions.try_recv()
                && lane.is_ready()
            {
                self.last_stream = stream.get();
                assoc.reconnect_regional(lane, port.roles, member, stream, now);
            }
            assoc.step(actor, self.remote, &mut self.last_revision, now);
        } else if let Some(links) = &mut actor.links
            && let Some(stream) = self.last_stream.checked_add(1).and_then(NonZeroU64::new)
        {
            if let Ok((network, lane)) = self.transport.open(links, self.remote.0) {
                let mut association = Association::new(
                    network,
                    lane,
                    self.transport.declarations(),
                    links.clock_origin(),
                    now,
                );
                self.last_stream = stream.get();
                association.stream = stream;
                if let Transport::Regional { port, .. } = &self.transport {
                    association.mesh_outbound = Some(mesh::Outbound::new(member, port.roles.1));
                }
                self.association = Some(association);
            } else {
                self.next_step = now.checked_add(REOPEN).unwrap_or(now);
            }
        }
    }
}

impl Association {
    fn new(
        network: NetworkId,
        lane: PeerControl,
        declarations: Option<Exchange>,
        epoch: Instant,
        now: Instant,
    ) -> Self {
        Self {
            network,
            lane,
            declarations,
            mesh_outbound: None,
            stream: NonZeroU64::MIN,
            awaiting_wan_refresh: false,
            catalog_reads: client::Reads::default(),
            epoch,
            session: None,
            reader: transfer::Reader::default(),
            sender: transfer::Sender::default(),
            pending_image: None,
            standby_cursor: 0,
            data_expectations: Vec::new(),
            repair_lookups: RepairLookups::default(),
            repair_interests: Vec::new(),
            local_wan_receipts: Box::default(),
            local_wan_sources: Box::default(),
            offer_cut: 0,
            wan_publishers: super::relay::wan_publisher::PublisherSends::default(),
            wan_repairs: super::relay::wan_repair::RepairSends::default(),
            media_repairs: media_repair::RepairRequests::default(),
            media_receives: media_repair::RepairReceives::default(),
            wan_transit: wan_transit::Sends::default(),
            transit_advertisements: Vec::new(),
            exports: Vec::new(),
            remote_wan: None,
            interest_after: None,
            wan_refresh: WanRefresh::new(now),
            refresh: now,
            live_until: now.checked_add(LIVENESS).unwrap_or(now),
        }
    }

    fn has_live_view(&self, now: Instant) -> bool {
        now < self.live_until
            && (self.lane.is_ready()
                || (self.mesh_outbound.is_some()
                    && self
                        .remote_wan
                        .as_ref()
                        .is_some_and(|(until, _)| now < *until)))
    }

    fn reconnect_regional(
        &mut self,
        lane: PeerControl,
        roles: (Role, Role),
        member: mesh::Member,
        stream: NonZeroU64,
        now: Instant,
    ) {
        self.lane = lane;
        self.stream = stream;
        self.declarations = Some(Exchange::new(roles.0, roles.1));
        self.mesh_outbound = Some(mesh::Outbound::new(member, roles.1));
        self.catalog_reads = client::Reads::default();
        self.reader = transfer::Reader::default();
        self.sender = transfer::Sender::default();
        self.pending_image = None;
        self.awaiting_wan_refresh = true;
        self.refresh = now;
    }

    fn catalog_declaration(
        &self,
        room: ProjectRoomKey,
        now: Instant,
    ) -> Option<(DeclarationId, Finalized)> {
        if !self.lane.is_ready() || now >= self.live_until || self.session.is_none() {
            return None;
        }
        let cut = self.declarations.as_ref()?.ready(
            Declaration {
                project: room.project,
                room: room.room,
                kind: Kind::Queryable,
            },
            now,
        )?;
        let declaration = DeclarationId {
            stream: self.stream,
            interest: cut.interest,
            changed_at: cut.changed_at,
        };
        Some((declaration, cut))
    }

    fn catalog_orders(
        &self,
        actor: &ControllerActor,
        remote: (NodeId, NonZeroU64),
        now: Instant,
    ) -> Vec<client::Order> {
        actor
            .catalog
            .demands(actor, remote)
            .filter_map(|demand| {
                let (declaration, cut) = self.catalog_declaration(demand.watch.scope.room, now)?;
                cut.present.then_some(client::Order {
                    demand,
                    declaration,
                })
            })
            .collect()
    }

    fn drive_catalog_reads(
        &mut self,
        actor: &mut ControllerActor,
        remote: (NodeId, NonZeroU64),
        expires_at: pulsebeam_link::LinkTime,
        now: Instant,
    ) {
        if self.declarations.is_none() {
            return;
        }
        let orders = self.catalog_orders(actor, remote, now);
        self.catalog_reads.prune(&orders, now, |watch| {
            actor.catalog.notify(watch, CatalogChange::Changed);
        });
        self.catalog_reads
            .send(&orders, now, |id, order, after, revision| {
                let watch = order.demand.watch;
                let Some(discover) = actor.catalog.discover_for(actor, watch) else {
                    return false;
                };
                let get = neighbor::Get {
                    project: watch.scope.room.project,
                    discover,
                    interest: order.declaration.interest,
                    after,
                    revision,
                };
                get.encode(id)
                    .is_some_and(|frame| self.lane.try_send(frame, expires_at).is_ok())
            });
    }

    fn start_ownership_image(
        &mut self,
        actor: &mut ControllerActor,
        revision: NonZeroU64,
        now: Instant,
    ) {
        self.pending_image = None;
        if let Ok(snapshot) = actor.local_endpoint_snapshot()
            && let Some(expires) = now.checked_add(FRAME_TTL)
        {
            let wan_samples = actor.sample_wan_publisher_anchors(&snapshot);
            if !wan_samples.is_empty() {
                self.pending_image = Some(PendingImage {
                    revision,
                    expires,
                    wan_samples,
                    wan_sources: Vec::new(),
                });
                return;
            }
        }
        self.set_ownership_image(actor, revision, Vec::new(), now);
    }

    fn flush_ownership_image(&mut self, actor: &mut ControllerActor, now: Instant) {
        let Some(mut pending) = self.pending_image.take() else {
            return;
        };
        if now < pending.expires {
            pending
                .wan_samples
                .retain_mut(|((room, publisher), receipt)| match receipt.try_recv() {
                    Ok(Some(sample)) => {
                        pending.wan_sources.push(
                            WanSourceWindow::from_sample(*room, *publisher, sample.sequence)
                                .with_projection(sample.timing)
                                .with_ladder(sample.ladder),
                        );
                        false
                    }
                    Err(oneshot::error::TryRecvError::Empty) => true,
                    _ => false,
                });
            if !pending.wan_samples.is_empty() {
                self.pending_image = Some(pending);
                return;
            }
        } else {
            pending.wan_sources.clear();
        }
        self.set_ownership_image(actor, pending.revision, pending.wan_sources, now);
    }

    fn next_offer_cut(&mut self) -> Option<NonZeroU64> {
        let cut = NonZeroU64::new(self.offer_cut.checked_add(1)?)?;
        self.offer_cut = cut.get();
        Some(cut)
    }

    fn set_ownership_image(
        &mut self,
        actor: &mut ControllerActor,
        revision: NonZeroU64,
        mut wan_sources: Vec<WanSourceWindow>,
        now: Instant,
    ) {
        let Some(Authority::Provisioned(trust)) = actor.receipt_trust.as_ref() else {
            return;
        };
        let Ok(snapshot) = actor.local_endpoint_snapshot() else {
            return;
        };
        wan_sources.retain(|source| {
            source.present_in(&snapshot)
                && actor
                    .lifecycle
                    .network_publisher_anchor(
                        source.room,
                        source.publisher.publication,
                        &actor.core.registry,
                    )
                    .is_some_and(|(_, current)| current == source.publisher)
        });
        if self.mesh_outbound.is_some()
            && let Some(mesh) = &actor.regional_mesh
        {
            let (origin, _) = mesh.identity();
            for source in &mut wan_sources {
                source.origin = Some(origin);
            }
        }
        let mut wan_sources = super::relay::wan_source::plan_windows(
            wan_sources,
            &self.local_wan_sources,
            super::relay::LIMIT,
        )
        .into_vec();
        if self.mesh_outbound.is_some() {
            wan_sources.retain_mut(|source| {
                if source.offer_cut.is_none() {
                    source.offer_cut = self.next_offer_cut();
                }
                source.offer_cut.is_some()
            });
        }
        let mut snapshot = snapshot.into_vec();
        for advertisement in self
            .transit_advertisements
            .iter()
            .filter(|ad| now < ad.expires)
        {
            if wan_sources.len() >= super::relay::LIMIT {
                break;
            }
            if let Some((_, fact)) = snapshot
                .iter()
                .find(|(key, _)| *key == advertisement.endpoint.0)
            {
                if fact != &advertisement.endpoint.1 {
                    continue;
                }
            } else {
                snapshot.push(advertisement.endpoint.clone());
            }
            if !wan_sources
                .iter()
                .any(|source| source.same_window(advertisement.source))
            {
                wan_sources.push(advertisement.source);
            }
        }
        snapshot.sort_by_key(|(key, _)| *key);
        let snapshot = snapshot.into_boxed_slice();
        let wan_sources = wan_sources.into_boxed_slice();
        let image = wire::encode_wan(
            revision,
            trust.checkpoint(),
            &snapshot,
            wire::WanRoutes {
                offers: self
                    .local_wan_receipts
                    .iter()
                    .copied()
                    .map(Into::into)
                    .collect(),
                sources: wan_sources.clone(),
                retired: self.wan_refresh.retired.clone(),
            },
        );
        tracing::debug!(
            network = self.network.epoch(), revision, endpoints = snapshot.len(),
            sources = wan_sources.len(), exports = self.exports.len(), acks = self.local_wan_receipts.len(),
            bytes = ?image.as_ref().map(Vec::len), "WAN ownership image produced"
        );
        if let Ok(bytes) = image {
            self.local_wan_sources = wan_sources;
            self.sender.set_image(now, bytes);
        } else {
            self.local_wan_sources = Box::default();
        }
    }

    fn wan_retired_labels(&self) -> Vec<[u8; 4]> {
        // Keep reporting a consumed label while the peer still advertises it.
        // A refreshed image of the same source is not a new transport permit.
        self.remote_wan
            .as_ref()
            .map_or_else(Vec::new, |(_, image)| {
                image
                    .routes
                    .iter()
                    .filter_map(|ack| {
                        let label = ack.window.ssrc();
                        (self.wan_publishers.retired(label) || self.wan_transit.retired(label))
                            .then_some(label.get().to_be_bytes())
                    })
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect()
            })
    }

    fn receive_repair_control(
        &mut self,
        actor: &mut ControllerActor,
        frame: fabric_data::RepairFrame,
        now: Instant,
    ) {
        use fabric_data::RepairAction;
        match frame.action {
            RepairAction::Cancel => {
                if let Some(binding) = self.repair_lookups.cancel(frame, now) {
                    self.wan_repairs.cancel(actor, binding);
                }
            }
            RepairAction::Lookup => {
                if let Some(lookup) = self.wan_publishers.repair_lookup(self.network, frame, now) {
                    self.repair_lookups
                        .lookup(frame, now, || actor.stage_repair_lookups(vec![lookup]));
                }
            }
            RepairAction::Located { sequence } => {
                if let Some(pending) = self
                    .repair_interests
                    .iter_mut()
                    .find(|pending| pending.accepts_lookup_reply(frame, now))
                {
                    pending.state = actor
                        .stage_wan_repair_receive(now, pending.interest, sequence)
                        .map_or(
                            RepairInterestState::Cancelled,
                            RepairInterestState::WanRepair,
                        );
                }
            }
            RepairAction::Unavailable => {
                if let Some(index) = self
                    .repair_interests
                    .iter()
                    .position(|pending| pending.accepts_lookup_reply(frame, now))
                {
                    let pending = self.repair_interests.swap_remove(index);
                    actor.stage_unavailable_network_repair(pending.interest, now);
                }
            }
        }
    }

    fn receive_media_repair_demand(
        &mut self,
        actor: &mut ControllerActor,
        demand: media_repair::Demand,
        now: Instant,
    ) {
        use crate::route::fabric::relay::{MediaRepairLookup, PeerRepairLookup};
        let publisher = self
            .wan_publishers
            .media_repair_grant(self.network, demand, now);
        let relay = self
            .wan_transit
            .media_repair_grant(self.network, demand, now);
        let ((Some(grant), None) | (None, Some(grant))) = (publisher, relay) else {
            return;
        };
        self.media_repairs.demand(demand, grant, now, |parent| {
            actor
                .stage_repair_lookups(vec![PeerRepairLookup::Media(MediaRepairLookup {
                    parent,
                    position: demand.position,
                })])
                .pop()
        });
    }

    fn receive_wan_repair_ready(
        &mut self,
        actor: &mut ControllerActor,
        scope: fabric_data::Scope,
        bytes: &[u8],
        now: Instant,
    ) {
        use crate::route::fabric::{hop_expectations::Path, relay::PeerRepairLookup};
        let Some((route, parent)) = self.wan_publishers.repair_parent(self.network, scope, now)
        else {
            return;
        };
        let Some(ready) = fabric_data::WanRepairReady::decode(bytes, parent.window.emission())
        else {
            return;
        };
        if parent
            .window
            .repair_window(ready.sequence, ready.window.ssrc())
            .ok()
            != Some(ready.window)
        {
            return;
        }
        self.repair_lookups.ready_matching(ready, now, |located| {
            matches!(located.lookup.as_ref(), PeerRepairLookup::Publisher(lookup)
                if lookup.route.matches(route) && lookup.branch == ready.branch && lookup.request == ready.request)
        }, |mut located| {
            located.expires = located.expires.min(route.tx.expires());
            self.wan_repairs.start(actor, self.network, parent, ready, located, now)
        });
    }

    fn flush_repair_interests(
        &mut self,
        actor: &mut ControllerActor,
        now: Instant,
        expires_at: LinkTime,
    ) {
        use fabric_data::RepairAction;
        let lane = &self.lane;
        self.repair_interests.retain_mut(|pending| {
            if now >= pending.interest.expires {
                actor.cancel_peer_repair_receive(pending.interest);
                return false;
            }
            let demand_ttl = pending
                .interest
                .expires
                .saturating_duration_since(self.epoch);
            let deadline = expires_at.min(LinkTime::from_nanos(
                u64::try_from(demand_ttl.as_nanos()).unwrap_or(u64::MAX),
            ));
            let action = match &pending.state {
                RepairInterestState::Lookup { .. } => RepairAction::Lookup,
                RepairInterestState::Cancelled => {
                    actor.cancel_peer_repair_receive(pending.interest);
                    RepairAction::Cancel
                }
                RepairInterestState::WanRepair(child) => {
                    let receipt = actor
                        .links
                        .as_ref()
                        .map_or(std::task::Poll::Ready(None), |links| {
                            links.wan_repair_receive_state(pending.interest, *child, now)
                        });
                    match receipt {
                        std::task::Poll::Pending => {}
                        std::task::Poll::Ready(Some(window)) => {
                            if let Some(bytes) = fabric_data::WanRepairReady::from_install(
                                pending.interest.parent,
                                *child,
                                window,
                            )
                            .and_then(fabric_data::WanRepairReady::encode)
                            {
                                let _ = lane.try_send(bytes, deadline);
                            }
                        }
                        std::task::Poll::Ready(None) => {
                            pending.state = RepairInterestState::Cancelled;
                        }
                    }
                    return true;
                }
            };
            let sent = pending
                .frame(action)
                .encode()
                .is_some_and(|bytes| lane.try_send(bytes, deadline).is_ok());
            if sent && let RepairInterestState::Lookup { sent } = &mut pending.state {
                *sent = true;
            }
            action != RepairAction::Cancel || !sent
        });
    }

    fn flush_repair_lookups(&mut self, now: Instant, expires_at: LinkTime) {
        let lane = &self.lane;
        self.repair_lookups.flush(now, |response| {
            response
                .encode()
                .is_some_and(|bytes| lane.try_send(bytes, expires_at).is_ok())
        });
    }

    fn receive_data_expectation(
        &mut self,
        actor: &mut ControllerActor,
        frame: fabric_data::Frame,
        now: Instant,
    ) {
        let Some(mut expires) = frame.lease().and_then(|lease| now.checked_add(lease)) else {
            return;
        };
        let Some(deadline) = actor
            .links
            .as_ref()
            .and_then(|links| links.wan_data_receive_deadline(self.network, frame.scope, now))
        else {
            return;
        };
        expires = expires.min(deadline);
        if self.data_expectations.len() >= EXPECTATION_BUDGET
            || self.data_expectations.iter().any(|pending| {
                pending.frame.scope == frame.scope && pending.frame.unit == frame.unit
            })
        {
            return;
        }
        if let Some(receipts) = actor.stage_peer_data_expectation(self.network, frame, now) {
            self.data_expectations.push(PendingExpectation {
                frame,
                expires,
                receipts,
                failed: false,
            });
        }
    }

    fn flush_data_expectations(
        &mut self,
        actor: &ControllerActor,
        now: Instant,
        expires_at: LinkTime,
    ) {
        use crate::route::fabric::local::PropagationOutcome;
        let lane = &self.lane;
        self.data_expectations.retain_mut(|pending| {
            if now >= pending.expires {
                return false;
            }
            let Some(deadline) = actor.links.as_ref().and_then(|links| {
                links.wan_data_receive_deadline(self.network, pending.frame.scope, now)
            }) else {
                return false;
            };
            let ttl = pending
                .expires
                .min(deadline)
                .saturating_duration_since(self.epoch);
            let expires_at = expires_at.min(LinkTime::from_nanos(
                u64::try_from(ttl.as_nanos()).unwrap_or(u64::MAX),
            ));
            pending
                .receipts
                .retain_mut(|receipt| match receipt.try_recv() {
                    Ok(accepted) => {
                        pending.failed |= !accepted;
                        false
                    }
                    Err(oneshot::error::TryRecvError::Empty) => true,
                    Err(oneshot::error::TryRecvError::Closed) => {
                        pending.failed = true;
                        false
                    }
                });
            if !pending.failed && !pending.receipts.is_empty() {
                return true;
            }
            let outcome = if pending.failed {
                PropagationOutcome::Failed
            } else {
                PropagationOutcome::Confirmed
            };
            let response = fabric_data::Frame {
                action: fabric_data::Action::Observed(outcome),
                ..pending.frame
            };
            response
                .encode()
                .is_some_and(|bytes| lane.try_send(bytes, expires_at).is_err())
        });
    }

    fn send_direct_interests(
        &mut self,
        actor: &mut ControllerActor,
        expires_at: LinkTime,
        now: Instant,
    ) -> bool {
        let Some((_, views)) = &self.remote_wan else {
            return false;
        };
        let wall = super::SystemTime::now();
        let Some(limit_ms) = wall.checked_add(interest::TTL).and_then(interest::millis) else {
            return false;
        };
        let Some(routes) = actor
            .links
            .as_mut()
            .and_then(|links| links.relay_targets(self.network, relay::LIMIT))
        else {
            return false;
        };
        let want_probe = !self.reader.pending(now) && now >= self.wan_refresh.pull_after;
        let mut probe = false;
        let after = self.interest_after;
        let mut batch = Vec::new();
        let mut frames = 0;
        for (&key, _) in routes
            .iter()
            .filter(|(key, _)| after.is_none_or(|after| **key > after))
        {
            self.interest_after = Some(key);
            let Some((room, publication)) = key.forward() else {
                continue;
            };
            if actor.lifecycle.ingress_peer(room, publication) != Some(self.network) {
                continue;
            }
            let Some(assertion) = actor
                .lifecycle
                .source_exact(room, publication)
                .and_then(|track| interest::Assertion::for_track(actor, track, wall, limit_ms))
            else {
                continue;
            };
            probe |= want_probe && !views.sources.iter().any(|source| source.scope() == key);
            batch.push(assertion);
            if batch.len() == interest::BATCH {
                if interest::encode(&batch)
                    .is_none_or(|bytes| self.lane.try_send(bytes, expires_at).is_err())
                {
                    return probe;
                }
                batch.clear();
                frames += 1;
                if frames == 4 {
                    return probe;
                }
            }
        }
        if let Some(bytes) = interest::encode(&batch) {
            let _ = self.lane.try_send(bytes, expires_at);
        }
        self.interest_after = None;
        probe
    }

    fn step(
        &mut self,
        actor: &mut ControllerActor,
        remote: (NodeId, NonZeroU64),
        last_revision: &mut u64,
        now: Instant,
    ) {
        if !self.lane.is_ready() {
            return;
        }
        if self
            .remote_wan
            .as_ref()
            .is_some_and(|(expires, _)| now >= *expires)
        {
            self.remote_wan = None;
        }
        let session = if let Some(session) = self.session {
            session
        } else {
            let session = match actor.open_ownership_peer(remote.0, remote.1, Some(self.network)) {
                Ok(session) => session,
                Err(error) => {
                    tracing::debug!(?remote, ?error, "WAN ownership open rejected");
                    return;
                }
            };
            tracing::debug!(?remote, "WAN ownership opened");
            self.session = Some(session);
            session
        };
        let Some(expires_at) = now.checked_add(FRAME_TTL).and_then(|at| {
            u64::try_from(at.saturating_duration_since(self.epoch).as_nanos())
                .ok()
                .map(LinkTime)
        }) else {
            return;
        };
        if self.awaiting_wan_refresh {
            // Existing media can drain on its bounded leases, but an expired
            // receive must not be re-advertised on the replacement stream.
            self.local_wan_receipts = actor
                .links
                .as_mut()
                .map_or_else(Box::default, |links| links.wan_offers(self.network));
        } else {
            let sources = self
                .remote_wan
                .as_ref()
                .map(|(deadline, image)| (*deadline, image.sources.as_ref()));
            let regional_peer = self.mesh_outbound.as_ref().map(|_| mesh::Member {
                node: remote.0,
                generation: remote.1,
            });
            super::relay::reconcile_wan_receives(
                actor,
                self.network,
                session,
                regional_peer,
                sources,
                now,
            );
            let acknowledged = self
                .remote_wan
                .as_ref()
                .map(|(deadline, image)| (*deadline, image.routes.as_ref()));
            self.wan_publishers.step(
                actor,
                self.network,
                &self.local_wan_sources,
                acknowledged,
                now,
            );
            self.wan_transit.step(
                actor,
                self.network,
                &self.exports,
                &self.local_wan_sources,
                acknowledged,
                now,
            );
            if actor.receipt_trust.as_ref().is_some_and(Authority::ready) {
                self.media_receives
                    .flush(actor, &self.lane, self.epoch, now, expires_at);
                self.media_repairs.flush(now, |parent, ready, located| {
                    self.wan_repairs.start(
                        actor,
                        self.network,
                        parent,
                        ready.send_permit(),
                        located,
                        now,
                    );
                });
            }
            self.wan_repairs.step(actor, now);
            let offers = actor
                .links
                .as_mut()
                .map_or_else(Box::default, |links| links.wan_offers(self.network));
            if self
                .local_wan_receipts
                .iter()
                .any(|ack| !offers.contains(ack))
            {
                self.sender.invalidate();
            }
            if self.local_wan_receipts != offers {
                self.wan_refresh.announce = true;
            }
            self.local_wan_receipts = offers;
            let retired = self.wan_retired_labels();
            if self.wan_refresh.retired != retired {
                self.wan_refresh.announce |= !retired.is_empty();
                self.wan_refresh.retired = retired;
            }
            if self.wan_refresh.announce
                && now >= self.wan_refresh.announce_after
                && self
                    .lane
                    .try_send(transfer::WAN_IMAGE_CHANGED.into(), expires_at)
                    .is_ok()
            {
                self.wan_refresh.announce = false;
                self.wan_refresh.announce_after =
                    now.checked_add(WAN_REFRESH_MIN_INTERVAL).unwrap_or(now);
            }
        }
        self.flush_repair_lookups(now, expires_at);
        self.flush_data_expectations(actor, now, expires_at);
        self.flush_repair_interests(actor, now, expires_at);
        self.flush_ownership_image(actor, now);
        let direct_interest = self.mesh_outbound.is_none()
            && actor.receipt_trust.as_ref().is_some_and(Authority::ready);
        let probe = direct_interest && self.send_direct_interests(actor, expires_at, now);
        let mut interests = Vec::new();
        // Schedule the local pull before responding to the peer's bulk window.
        // Both directions remain bounded even when media or the peer is stalled.
        if self.wan_refresh.pull && now >= self.wan_refresh.pull_after {
            self.wan_refresh.pull = false;
            self.wan_refresh.pull_after = now.checked_add(WAN_REFRESH_MIN_INTERVAL).unwrap_or(now);
            // A replacement grant may have invalidated the image in the current round.
            let _ = self.reader.start(now);
        } else if probe || (now >= self.refresh && !self.reader.pending(now)) {
            if probe {
                self.wan_refresh.pull_after =
                    now.checked_add(WAN_REFRESH_MIN_INTERVAL).unwrap_or(now);
            }
            let _ = self.reader.start(now);
        }
        if let Some(frame) = self.reader.pull(now) {
            let _ = self.lane.try_send(frame, expires_at);
        }
        if let Some(offer) = actor.publisher_handoffs.offer(remote, now) {
            let _ = self.lane.try_send(offer, expires_at);
        }
        if !actor.standby_outbox.is_empty()
            && let Some(Authority::Provisioned(trust)) = &actor.receipt_trust
            && actor.provisioned_claim_views_ready(trust, now)
        {
            actor
                .standby_outbox
                .send(&self.lane, expires_at, &mut self.standby_cursor);
        }
        let mut sampled = false;
        let mut catalog_read = false;
        for _ in 0..RECEIVE_BUDGET {
            let Some(bytes) = self.lane.try_recv() else {
                break;
            };
            if self
                .declarations
                .as_mut()
                .is_some_and(|exchange| exchange.receive(&bytes, now))
            {
                continue;
            }
            if let (Some(regional_mesh), Some(peer)) =
                (&mut actor.regional_mesh, &self.mesh_outbound)
                && regional_mesh.receive(peer, &bytes, now)
            {
                continue;
            }
            if super::super::regional_routing::recognizes(&bytes) {
                if self.mesh_outbound.is_some()
                    && let Some(routing) = &mut actor.regional_routing
                {
                    routing.enqueue(
                        mesh::Member {
                            node: remote.0,
                            generation: remote.1,
                        },
                        self.stream,
                        bytes,
                    );
                }
                continue;
            }
            if neighbor::recognizes(&bytes) {
                if actor.regional_routing.is_none()
                    && let Some(declarations) = &self.declarations
                {
                    match neighbor::decode(&bytes) {
                        Some(neighbor::MessageOnStream::Get(id, get)) if !catalog_read => {
                            catalog_read = true;
                            if let Some(reply) = get.serve(id, actor, declarations, now) {
                                let _ = self.lane.try_send(reply, expires_at);
                            }
                        }
                        Some(neighbor::MessageOnStream::Reply(id, result)) => {
                            let orders = self.catalog_orders(actor, remote, now);
                            if let Some((watch, result)) =
                                self.catalog_reads.receive(id, result, now, &orders)
                            {
                                actor.catalog.receive_remote(watch, result);
                            }
                        }
                        _ => {}
                    }
                }
                continue;
            }
            if let Some(frame) = StandbyFrame::decode(&bytes) {
                let _ =
                    actor.receive_standby_control(session, frame, now, super::SystemTime::now());
                continue;
            }
            if let Some(frame) = HandoffFrame::decode(&bytes) {
                if let Some(ack) = actor.receive_publisher_handoff(session, frame) {
                    let _ = self.lane.try_send(ack, expires_at);
                }
                continue;
            }
            if direct_interest && let Some(assertions) = interest::decode(&bytes) {
                if let Some(wall) = interest::millis(super::SystemTime::now()) {
                    interests.extend(
                        assertions
                            .into_iter()
                            .filter_map(|item| item.resolve(actor, now, wall)),
                    );
                }
                continue;
            }
            if actor.receipt_trust.as_ref().is_some_and(Authority::ready) {
                if let Some(demand) = media_repair::Demand::decode(&bytes) {
                    self.receive_media_repair_demand(actor, demand, now);
                    continue;
                }
                if let Some(demand) = media_repair::Ready::read_demand(&bytes) {
                    self.media_repairs.ready(&bytes, demand, now);
                    continue;
                }
            }
            if let Some(scope) = fabric_data::WanRepairReady::read_scope(&bytes) {
                self.receive_wan_repair_ready(actor, scope, &bytes, now);
                continue;
            }
            if let Some(frame) = fabric_data::RepairFrame::decode(&bytes) {
                self.receive_repair_control(actor, frame, now);
                continue;
            }
            if let Some(frame) = fabric_data::Frame::decode(&bytes) {
                match frame.action {
                    fabric_data::Action::Expect { .. } => {
                        self.receive_data_expectation(actor, frame, now);
                    }
                    fabric_data::Action::Observed(_) => {
                        if let Some(observation) =
                            self.wan_publishers
                                .data_observation(self.network, frame, now)
                        {
                            actor.stage_publisher_data_observations(vec![observation]);
                        }
                    }
                }
                continue;
            }
            match Frame::decode(&bytes) {
                Some(Frame::WanImageChanged) => self.wan_refresh.pull = true,
                Some(Frame::Pull(pull)) => {
                    if !sampled
                        && self.sender.needs_image(pull)
                        && let Some(revision) =
                            last_revision.checked_add(1).and_then(NonZeroU64::new)
                    {
                        sampled = true;
                        *last_revision = revision.get();
                        tracing::debug!(?remote, revision, "WAN ownership pull received");
                        self.start_ownership_image(actor, revision, now);
                    }
                    for frame in self.sender.respond(now, pull) {
                        if self.lane.try_send(frame, expires_at).is_err() {
                            break;
                        }
                    }
                }
                Some(Frame::Chunk(chunk)) => {
                    if let Some((bytes, deadline)) = self.reader.receive(now, chunk)
                        && let Ok(image) = wire::decode_wan(&bytes)
                        && self.lane.is_ready()
                        && actor.links.is_some()
                        && actor
                            .apply_ownership_snapshot(
                                session,
                                image.revision,
                                image.endpoints,
                                deadline,
                                Some(image.authority),
                            )
                            .is_ok()
                    {
                        let wan = image.wan;
                        if let Some(links) = &mut actor.links
                            && links.accept_wan_retirements(self.network, &wan.retired)
                        {
                            self.sender.invalidate();
                            self.wan_refresh.announce = true;
                            self.local_wan_receipts = links.wan_offers(self.network);
                        }
                        tracing::debug!(
                            ?remote,
                            revision = image.revision,
                            "WAN ownership image installed"
                        );
                        if now < deadline {
                            for source in &wan.sources {
                                source.record_timing_quality();
                            }
                        }
                        self.remote_wan = Some((deadline, wan));
                        self.awaiting_wan_refresh = false;
                        self.refresh = now.checked_add(REFRESH).unwrap_or(now);
                        self.live_until = now.checked_add(LIVENESS).unwrap_or(now);
                    }
                }
                None => {}
            }
        }
        if !interests.is_empty() {
            actor.stage_publisher_interests(interests);
        }
        if actor.regional_routing.is_none() {
            self.drive_catalog_reads(actor, remote, expires_at, now);
        }
        if let Some(exchange) = &mut self.declarations {
            let desired = actor
                .catalog
                .declaration_rooms(actor)
                .into_iter()
                .map(|room| Declaration {
                    project: room.project,
                    room: room.room,
                    kind: Kind::Queryable,
                })
                .collect();
            exchange.step(
                &desired,
                |declaration| {
                    let room = ProjectRoomKey {
                        project: declaration.project,
                        room: declaration.room,
                    };
                    match declaration.kind {
                        Kind::Queryable => actor.core.registry.get_room_shard(&room).is_some(),
                        Kind::Subscriber => actor.lifecycle.has_local_subscriber(room),
                    }
                },
                &self.lane,
                expires_at,
                now,
            );
        }
        if let (Some(mesh), Some(outbound)) = (&actor.regional_mesh, &mut self.mesh_outbound) {
            mesh.flush(outbound, &self.lane, expires_at, now);
        }
    }
}

impl ControllerActor {
    #[cfg(test)]
    pub(in crate::control::controller) fn install_catalog_fixture(
        &mut self,
        remote: NodeId,
        roles: (Role, Role),
        lane: PeerControl,
        now: Instant,
    ) {
        let network = self
            .links
            .get_or_insert_with(|| {
                crate::control::link::Links::new(
                    pulsebeam_link::LinkControl::new(|| pulsebeam_link::LinkTime::from_nanos(0))
                        .unwrap(),
                    Vec::new(),
                    now,
                )
            })
            .reserve_wan_peer()
            .unwrap();
        let remote = (remote, NonZeroU64::MIN);
        let mut peer = Peer::new(remote, Transport::CatalogFixture, None, now);
        let mut association = Association::new(
            network,
            lane,
            Some(Exchange::new(roles.0, roles.1)),
            now,
            now,
        );
        // There is no WAN ownership snapshot renewal in this catalog fixture.
        // Regional declarations and catalog reads still use production deadlines.
        association.live_until = now + Duration::from_secs(60);
        association.session = Some(self.open_ownership_peer(remote.0, remote.1, None).unwrap());
        peer.association = Some(association);
        self.provisioned_peers.push(peer);
    }

    pub(crate) fn add_provisioned_peer(
        &mut self,
        spec: ProvisionedPeer,
        wan_host: crate::link::wan_host::Provisioner,
        regional: Option<mesh::Port>,
    ) -> anyhow::Result<()> {
        let issuer = |(node, incarnation)| Issuer {
            trust_domain: spec.authority_epoch,
            node,
            incarnation,
        };
        let media = WanHost {
            peer: spec.transport,
            generation: spec.local.1,
            host: wan_host,
        };
        let transport = match regional {
            Some(port) => {
                anyhow::ensure!(
                    port.roles.0.adjacent(port.roles.1),
                    "invalid regional adjacency roles"
                );
                Transport::Regional { port, media }
            }
            None => Transport::Wan(media),
        };
        let mesh_identity = if let Transport::Regional { port, .. } = &transport {
            let role = port.roles.0;
            let local = mesh::Member {
                node: spec.local.0,
                generation: spec.local.1,
            };
            anyhow::ensure!(
                self.regional_mesh
                    .as_ref()
                    .is_none_or(|mesh| mesh.identity() == (local, role)),
                "inconsistent regional mesh identity"
            );
            Some((local, role))
        } else {
            None
        };
        let local = issuer(spec.local);
        let remote = issuer(spec.remote);
        match &mut self.receipt_trust {
            Some(Authority::Provisioned(trust)) => {
                anyhow::ensure!(
                    trust.matches_local(local, spec.receipt_key.public_key()),
                    "inconsistent provisioned receipt authority"
                );
                trust
                    .pin_peer(remote, spec.peer_receipt_key)
                    .map_err(|_| anyhow::anyhow!("invalid additional receipt authority"))?;
            }
            Some(Authority::Local(_)) => {
                anyhow::bail!("cannot mix local and provisioned receipt authority")
            }
            None => {
                let trust = Trust::new(local, spec.receipt_key, remote, spec.peer_receipt_key)
                    .map_err(|_| anyhow::anyhow!("invalid provisioned receipt authority"))?;
                self.receipt_trust = Some(Authority::Provisioned(Box::new(trust)));
            }
        }
        if let Some((local, role)) = mesh_identity {
            self.regional_mesh
                .get_or_insert_with(|| mesh::Exchange::new(local, role, Instant::now()));
        }
        self.provisioned_peers.push(Peer::new(
            spec.remote,
            transport,
            spec.relay_from,
            Instant::now(),
        ));
        Ok(())
    }

    pub(in crate::control) fn drive_provisioned_peers(&mut self, now: Instant) {
        if !self.standby_outbox.is_empty() {
            self.refresh_standby_outbox(now, super::SystemTime::now());
        }
        let mut peers = std::mem::take(&mut self.provisioned_peers);
        if let Some(mesh) = &mut self.regional_mesh {
            mesh.reconcile(
                peers
                    .iter()
                    .filter_map(|peer| peer.live_mesh_link(now))
                    .collect(),
                now,
            );
        }
        self.reconcile_wan_transit(&mut peers, now);
        for peer in &mut peers {
            peer.step(self, now);
        }
        self.provisioned_peers = peers;
        self.drive_regional_routing(now);
    }

    fn reconcile_wan_transit(&mut self, peers: &mut [Peer], now: Instant) {
        let is_router = self
            .regional_mesh
            .as_ref()
            .is_some_and(|mesh| mesh.identity().1 == Role::Router);
        if !is_router && !peers.iter().any(|peer| peer.relay_from.is_some()) {
            return;
        }
        let mut desired = HashSet::new();
        if let Some(links) = &mut self.links {
            links.reap();
        }
        for index in 0..peers.len() {
            let (before, rest) = peers.split_at_mut(index);
            let [child, after @ ..] = rest else { break };
            let child_member = child.mesh_member();
            let Some(outbound) = &mut child.association else {
                continue;
            };
            let regional = outbound.mesh_outbound.is_some();
            let mut advertisements = Vec::<wan_transit::Advertisement>::new();
            let mut exports = Vec::new();
            let authority = self.receipt_trust.as_ref().is_some_and(Authority::ready);
            for parent in before.iter().chain(after.iter()) {
                if (!regional || child.relay_from.is_some())
                    && child.relay_from != Some(parent.remote.0)
                {
                    continue;
                }
                let Some(upstream) = parent
                    .association
                    .as_ref()
                    .filter(|assoc| assoc.has_live_view(now))
                else {
                    continue;
                };
                let (Some(session), Some((deadline, image))) =
                    (upstream.session, &upstream.remote_wan)
                else {
                    continue;
                };
                if now >= *deadline || regional != upstream.mesh_outbound.is_some() {
                    continue;
                }
                let network = upstream.network;
                for &source in &image.sources {
                    if advertisements.len() >= super::relay::LIMIT {
                        break;
                    }
                    if regional {
                        let Some(path) = source
                            .origin
                            .and_then(|origin| self.regional_mesh.as_ref()?.path(origin))
                        else {
                            continue;
                        };
                        if !is_router
                            || !authority
                            || source.offer_cut.is_none()
                            || path.first() != Some(&parent.mesh_member())
                            || path.contains(&child_member)
                        {
                            continue;
                        }
                    }
                    let publication = source.publisher.publication;
                    let Some(canonical) = self.lifecycle.ingress_peer(source.room, publication)
                    else {
                        continue;
                    };
                    let Some((key, fact)) =
                        self.peer_ownership
                            .session_facts(session, now)
                            .find(|(key, fact)| {
                                source.matches_fact(**key, fact)
                                    && (canonical == network
                                        || (regional
                                            && self
                                                .peer_ownership
                                                .network_fact(canonical, **key, now)
                                                == Some(*fact)))
                            })
                    else {
                        continue;
                    };
                    let mut ad = wan_transit::Advertisement {
                        source,
                        endpoint: (*key, fact.clone()),
                        upstream: network,
                        upstream_cut: source.offer_cut,
                        expires: *deadline,
                    };
                    if regional {
                        if advertisements
                            .iter()
                            .any(|previous| previous.source.same_window(source))
                        {
                            continue;
                        }
                        ad.source.offer_cut = outbound
                            .transit_advertisements
                            .iter()
                            .find(|old| now < old.expires && old.same_source(&ad))
                            .and_then(|old| old.source.offer_cut)
                            .or_else(|| outbound.next_offer_cut());
                        if ad.source.offer_cut.is_none() {
                            continue;
                        }
                    }
                    let demanded = !regional
                        || (outbound.has_live_view(now)
                            && outbound.local_wan_sources.iter().any(|offered| {
                                offered.same_window(ad.source)
                                    && offered.offer_cut == ad.source.offer_cut
                            })
                            && outbound.remote_wan.as_ref().is_some_and(|(until, image)| {
                                now < *until
                                    && image.routes.iter().any(|&ack| ad.source.matches_ack(ack))
                            }));
                    if demanded
                        && desired.insert((source.room, publication, canonical))
                        && let Some(outcome) = self.lifecycle.set_wan_transit(
                            source.room,
                            publication,
                            canonical,
                            Some(wan_transit::SHARD),
                            &self.core.registry,
                            now,
                        )
                    {
                        self.publish_track_lifecycle(outcome);
                    }
                    if let Some((input, expires)) = demanded
                        .then(|| {
                            self.links.as_ref()?.wan_transit_input(
                                network,
                                source,
                                wan_transit::SHARD,
                                false,
                                now,
                            )
                        })
                        .flatten()
                    {
                        let expires = expires.min(ad.expires);
                        exports.push(wan_transit::Export {
                            source: ad.source,
                            input,
                            expires,
                        });
                        if !regional {
                            ad.expires = expires;
                        }
                    } else if !regional {
                        continue;
                    }
                    advertisements.push(ad);
                }
            }
            let same_offer = |old: &wan_transit::Advertisement,
                              new: &wan_transit::Advertisement| {
                old.same_source(new) && old.source.offer_cut == new.source.offer_cut
            };
            let withdrew = outbound
                .transit_advertisements
                .iter()
                .any(|old| !advertisements.iter().any(|new| same_offer(old, new)));
            if withdrew {
                outbound.sender.invalidate();
            }
            outbound.wan_refresh.announce |= withdrew
                || advertisements.iter().any(|new| {
                    !outbound
                        .transit_advertisements
                        .iter()
                        .any(|old| same_offer(old, new))
                });
            outbound.transit_advertisements = advertisements;
            outbound.exports = exports;
        }
        let mut retired: Vec<_> = self
            .lifecycle
            .wan_transit_demands()
            .filter(|demand| !desired.contains(demand))
            .collect();
        retired.sort_by_key(|&(room, publication, network)| {
            (
                super::relay::Key::new(room, publication, false),
                network.epoch(),
            )
        });
        for (room, publication, network) in retired {
            if let Some(outcome) = self.lifecycle.set_wan_transit(
                room,
                publication,
                network,
                None,
                &self.core.registry,
                now,
            ) {
                self.publish_track_lifecycle(outcome);
            }
        }
    }
}
