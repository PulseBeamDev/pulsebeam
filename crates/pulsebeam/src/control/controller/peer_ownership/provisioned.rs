//! Operator-provisioned point-to-point ownership observation over Link.
//! No address, certificate, or incarnation is learned from membership gossip.

use std::{collections::HashSet, num::NonZeroU64, time::Duration};

use pulsebeam_core::identity::NodeId;
use pulsebeam_link::{LinkTime, NetworkId, NetworkPeer, PeerControl};
use pulsebeam_runtime::{sync::oneshot, time::Instant};

use super::{
    ControllerActor, Session, fabric_data, media_repair,
    ownership::{
        receipt::{Authority, Issuer, epoch::Trust},
        wire,
    },
    relay::{Relays, wan_source::WanSourceWindow, wan_transit},
    transfer::{self, Frame},
};

const REFRESH: Duration = Duration::from_secs(3);
const REOPEN: Duration = Duration::from_secs(1);
const LIVENESS: Duration = Duration::from_secs(15);
const FRAME_TTL: Duration = Duration::from_millis(450);
const WAN_REFRESH_MIN_INTERVAL: Duration = Duration::from_millis(450);
const RECEIVE_BUDGET: usize = 16;
const EXPECTATION_BUDGET: usize = 32;
pub(super) const REPAIR_LOOKUP_BUDGET: usize = 32;
use crate::{
    cluster::ProvisionedPeer,
    route::fabric::{
        network_repair::{Interest, MAX_REPAIR_LIFETIME},
        relay::LocatedRepair,
    },
    shard_update::PublisherAnchor,
};

pub(in crate::control) struct Peer {
    remote: (NodeId, NonZeroU64),
    transport: Transport,
    relay_from: Option<NodeId>,
    association: Option<Association>,
    next_step: Instant,
    last_revision: u64,
}

enum Transport {
    ReverseDtls(NetworkPeer),
    Wan {
        peer: pulsebeam_link::NetworkPeer,
        generation: NonZeroU64,
        host: crate::link::wan_host::Provisioner,
        opus_source_budget: bool,
    },
}

impl Transport {
    fn open(
        &self,
        links: &mut crate::control::link::Links,
        remote: NodeId,
    ) -> Result<(NetworkId, PeerControl), pulsebeam_link::LinkError> {
        match self {
            Self::ReverseDtls(peer) => links.open_reverse_control_peer(peer.clone(), remote),
            Self::Wan {
                peer,
                generation,
                host,
                opus_source_budget,
            } => {
                let id = links.reserve_wan_peer()?;
                let (commands, lane) =
                    host.open(peer.clone(), id, *generation, links.clock_origin())?;
                links
                    .attach_wan_host(commands, *opus_source_budget)
                    .map_err(|_| pulsebeam_link::LinkError::Capacity)?;
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
    oneshot::Receiver<Option<u64>>,
);

struct PendingImage {
    revision: NonZeroU64,
    expires: Instant,
    wan_samples: Vec<WanSample>,
    wan_sources: Vec<WanSourceWindow>,
}

struct Association {
    network: NetworkId,
    is_wan: bool,
    lane: PeerControl,
    epoch: Instant,
    session: Option<Session>,
    reader: transfer::Reader,
    sender: transfer::Sender,
    pending_image: Option<PendingImage>,
    data_expectations: Vec<PendingExpectation>,
    repair_lookups: RepairLookups,
    repair_interests: Vec<RepairInterest>,
    relays: Relays,
    local_wan_receipts: Box<[super::relay::fabric::WanLiveRouteAck]>,
    local_wan_sources: Box<[WanSourceWindow]>,
    wan_publishers: super::relay::wan_publisher::PublisherSends,
    wan_repairs: super::relay::wan_repair::RepairSends,
    media_repairs: media_repair::RepairRequests,
    media_receives: media_repair::RepairReceives,
    wan_transit: wan_transit::Sends,
    exports: Vec<wan_transit::Export>,
    remote_wan: Option<(Instant, wire::WanOwnership)>,
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
        }
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
            association.is_wan
                && association.lane.is_ready()
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
            expectation.expires.min(expectation.route.expires_at),
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
            && assoc.is_wan
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
                && association.is_wan == interest.parent.window.is_some()
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

    fn step(&mut self, actor: &mut ControllerActor, now: Instant) {
        if self
            .association
            .as_ref()
            .is_some_and(|assoc| assoc.lane.is_closed() || now >= assoc.live_until)
        {
            // The socket owner has already fenced read/write. Withdraw the
            // ownership view on the actor, without enqueueing a close command.
            if let Some(assoc) = self.association.take() {
                actor.close_link_peer(assoc.network);
            }
            self.next_step = now.checked_add(REOPEN).unwrap_or(now);
        }
        if now < self.next_step {
            return;
        }
        self.next_step = now.checked_add(transfer::RETRY).unwrap_or(now);
        if let Some(assoc) = &mut self.association {
            assoc.step(actor, self.remote, &mut self.last_revision, now);
        } else if let Some(links) = &mut actor.links {
            if let Ok((network, lane)) = self.transport.open(links, self.remote.0) {
                self.association = Some(Association {
                    network,
                    is_wan: matches!(self.transport, Transport::Wan { .. }),
                    lane,
                    epoch: links.clock_origin(),
                    session: None,
                    reader: transfer::Reader::default(),
                    sender: transfer::Sender::default(),
                    pending_image: None,
                    data_expectations: Vec::new(),
                    repair_lookups: RepairLookups::default(),
                    repair_interests: Vec::new(),
                    relays: Relays::default(),
                    local_wan_receipts: Box::default(),
                    local_wan_sources: Box::default(),
                    wan_publishers: super::relay::wan_publisher::PublisherSends::default(),
                    wan_repairs: super::relay::wan_repair::RepairSends::default(),
                    media_repairs: media_repair::RepairRequests::default(),
                    media_receives: media_repair::RepairReceives::default(),
                    wan_transit: wan_transit::Sends::default(),
                    exports: Vec::new(),
                    remote_wan: None,
                    wan_refresh: WanRefresh::new(now),
                    refresh: now,
                    live_until: now.checked_add(LIVENESS).unwrap_or(now),
                });
            } else {
                self.next_step = now.checked_add(REOPEN).unwrap_or(now);
            }
        }
    }
}

impl Association {
    fn start_ownership_image(
        &mut self,
        actor: &mut ControllerActor,
        revision: NonZeroU64,
        now: Instant,
    ) {
        self.pending_image = None;
        if self.is_wan
            && let Ok(snapshot) = actor.local_endpoint_snapshot()
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
                    Ok(Some(sequence_anchor)) => {
                        pending.wan_sources.push(WanSourceWindow::from_sample(
                            *room,
                            *publisher,
                            sequence_anchor,
                        ));
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
        let mut wan_sources = super::relay::wan_source::plan_windows(
            wan_sources,
            &self.local_wan_sources,
            super::relay::LIMIT,
        )
        .into_vec();
        let mut snapshot = snapshot.into_vec();
        for export in self.exports.iter().filter(|export| now < export.expires) {
            if wan_sources.len() >= super::relay::LIMIT {
                break;
            }
            if let Some((_, fact)) = snapshot.iter().find(|(key, _)| *key == export.endpoint.0) {
                if fact != &export.endpoint.1 {
                    continue;
                }
            } else {
                snapshot.push(export.endpoint.clone());
            }
            if !wan_sources.contains(&export.source) {
                wan_sources.push(export.source);
            }
        }
        snapshot.sort_by_key(|(key, _)| *key);
        let snapshot = snapshot.into_boxed_slice();
        let wan_sources = wan_sources.into_boxed_slice();
        let image = wire::encode(
            revision,
            trust.checkpoint(),
            &snapshot,
            if self.is_wan {
                wire::RouteImage::Wan {
                    offers: self
                        .local_wan_receipts
                        .iter()
                        .copied()
                        .map(Into::into)
                        .collect(),
                    sources: wan_sources.clone(),
                    retired: self.wan_refresh.retired.clone(),
                }
            } else {
                wire::RouteImage::Legacy(self.relays.offers())
            },
        );
        if self.is_wan {
            tracing::debug!(
                network = self.network.epoch(), revision, endpoints = snapshot.len(),
                sources = wan_sources.len(), exports = self.exports.len(), acks = self.local_wan_receipts.len(),
                bytes = ?image.as_ref().map(Vec::len), "WAN ownership image produced"
            );
        }
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
            located.expires = located.expires.min(route.expires_at);
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
        if self.is_wan {
            let Some(deadline) = actor
                .links
                .as_ref()
                .and_then(|links| links.wan_data_receive_deadline(self.network, frame.scope, now))
            else {
                return;
            };
            expires = expires.min(deadline);
        }
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
            let expires_at = if self.is_wan {
                let Some(deadline) = actor.links.as_ref().and_then(|links| {
                    links.wan_data_receive_deadline(self.network, pending.frame.scope, now)
                }) else {
                    return false;
                };
                let ttl = pending
                    .expires
                    .min(deadline)
                    .saturating_duration_since(self.epoch);
                expires_at.min(LinkTime::from_nanos(
                    u64::try_from(ttl.as_nanos()).unwrap_or(u64::MAX),
                ))
            } else {
                expires_at
            };
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
                    if self.is_wan {
                        tracing::debug!(?remote, ?error, "WAN ownership open rejected");
                    }
                    return;
                }
            };
            if self.is_wan {
                tracing::debug!(?remote, "WAN ownership opened");
            }
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
        if self.is_wan {
            let sources = self
                .remote_wan
                .as_ref()
                .map(|(deadline, image)| (*deadline, image.sources.as_ref()));
            super::relay::reconcile_wan_receives(actor, self.network, session, sources, now);
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
        } else {
            self.relays.step(actor, self.network, now);
        }
        self.flush_repair_lookups(now, expires_at);
        self.flush_data_expectations(actor, now, expires_at);
        self.flush_repair_interests(actor, now, expires_at);
        self.flush_ownership_image(actor, now);
        // Schedule the local pull before responding to the peer's bulk window.
        // Both directions remain bounded even when media or the peer is stalled.
        if self.is_wan && self.wan_refresh.pull && now >= self.wan_refresh.pull_after {
            self.wan_refresh.pull = false;
            self.wan_refresh.pull_after = now.checked_add(WAN_REFRESH_MIN_INTERVAL).unwrap_or(now);
            // A replacement grant may have invalidated the image in the current round.
            let _ = self.reader.start(now);
        } else if now >= self.refresh && !self.reader.pending(now) {
            let _ = self.reader.start(now);
        }
        if let Some(frame) = self.reader.pull(now) {
            let _ = self.lane.try_send(frame, expires_at);
        }
        let mut sampled = false;
        for _ in 0..RECEIVE_BUDGET {
            let Some(bytes) = self.lane.try_recv() else {
                break;
            };
            if self.is_wan && actor.receipt_trust.as_ref().is_some_and(Authority::ready) {
                if let Some(demand) = media_repair::Demand::decode(&bytes) {
                    self.receive_media_repair_demand(actor, demand, now);
                    continue;
                }
                if let Some(demand) = media_repair::Ready::read_demand(&bytes) {
                    self.media_repairs.ready(&bytes, demand, now);
                    continue;
                }
            }
            if self.is_wan
                && let Some(scope) = fabric_data::WanRepairReady::read_scope(&bytes)
            {
                self.receive_wan_repair_ready(actor, scope, &bytes, now);
                continue;
            }
            if self.is_wan
                && let Some(frame) = fabric_data::RepairFrame::decode(&bytes)
            {
                self.receive_repair_control(actor, frame, now);
                continue;
            }
            if self.is_wan
                && let Some(frame) = fabric_data::Frame::decode(&bytes)
            {
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
                Some(Frame::WanImageChanged) => self.wan_refresh.pull |= self.is_wan,
                Some(Frame::Pull(pull)) => {
                    if !sampled
                        && self.sender.needs_image(pull)
                        && let Some(revision) =
                            last_revision.checked_add(1).and_then(NonZeroU64::new)
                    {
                        sampled = true;
                        *last_revision = revision.get();
                        if self.is_wan {
                            tracing::debug!(?remote, revision, "WAN ownership pull received");
                        }
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
                        && let Ok(image) = if self.is_wan {
                            let image = wire::decode_wan(&bytes);
                            tracing::debug!(?remote, bytes = bytes.len(), error = ?image.as_ref().err(), "WAN ownership image received");
                            image
                        } else {
                            wire::decode(&bytes, None)
                        }
                        && self.lane.is_ready()
                        && actor.links.is_some()
                        && (if self.is_wan {
                            image.wan.is_some()
                        } else {
                            image.matches_carriage(false)
                        })
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
                        if let Some(wan) = image.wan {
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
                            self.remote_wan = Some((deadline, wan));
                        } else {
                            self.relays.install(image.routes, deadline);
                            self.relays.step(actor, self.network, now);
                        }
                        self.refresh = now.checked_add(REFRESH).unwrap_or(now);
                        self.live_until = now.checked_add(LIVENESS).unwrap_or(now);
                    }
                }
                None => {}
            }
        }
    }
}

impl ControllerActor {
    pub(crate) fn add_provisioned_peer(
        &mut self,
        spec: ProvisionedPeer,
        wan_host: Option<crate::link::wan_host::Provisioner>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            spec.fabric_wan == wan_host.is_some(),
            "provisioned ownership carrier must match its socket owner"
        );
        let issuer = |(node, incarnation)| Issuer {
            trust_domain: spec.authority_epoch,
            node,
            incarnation,
        };
        let transport = if let Some(host) = wan_host {
            Transport::Wan {
                peer: spec.transport,
                generation: spec.local.1,
                host,
                opus_source_budget: spec.wan_opus_source_budget,
            }
        } else {
            Transport::ReverseDtls(spec.transport)
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
        self.provisioned_peers.push(Peer::new(
            spec.remote,
            transport,
            spec.relay_from,
            Instant::now(),
        ));
        Ok(())
    }

    pub(in crate::control) fn drive_provisioned_peers(&mut self, now: Instant) {
        let mut peers = std::mem::take(&mut self.provisioned_peers);
        self.reconcile_wan_transit(&mut peers, now);
        for peer in &mut peers {
            peer.step(self, now);
        }
        self.provisioned_peers = peers;
    }

    fn reconcile_wan_transit(&mut self, peers: &mut [Peer], now: Instant) {
        if !peers.iter().any(|peer| peer.relay_from.is_some()) {
            return;
        }
        let mut desired = HashSet::new();
        if let Some(links) = &mut self.links {
            links.reap();
        }
        for index in 0..peers.len() {
            let Some(from) = peers.get(index).and_then(|peer| peer.relay_from) else {
                continue;
            };
            let upstream = peers
                .iter()
                .find(|peer| peer.remote.0 == from)
                .and_then(|peer| peer.association.as_ref());
            let mut exports = Vec::new();
            if let Some(upstream) = upstream.filter(|assoc| assoc.lane.is_ready())
                && let Some(session) = upstream.session
                && let Some((deadline, image)) = &upstream.remote_wan
                && now < *deadline
            {
                let network = upstream.network;
                let candidates: Vec<_> = image
                    .sources
                    .iter()
                    .filter_map(|&source| {
                        if self
                            .lifecycle
                            .ingress_peer(source.room, source.publisher.publication)
                            != Some(network)
                        {
                            return None;
                        }
                        self.peer_ownership
                            .session_facts(session, now)
                            .find(|(key, fact)| source.matches_fact(**key, fact))
                            .map(|(key, fact)| (source, (*key, fact.clone())))
                    })
                    .take(super::relay::LIMIT)
                    .collect();
                for (source, endpoint) in candidates {
                    let publication = source.publisher.publication;
                    if desired.insert((source.room, publication, network))
                        && let Some(outcome) = self.lifecycle.set_wan_transit(
                            source.room,
                            publication,
                            network,
                            Some(wan_transit::SHARD),
                            &self.core.registry,
                            now,
                        )
                    {
                        tracing::debug!(
                            network = network.epoch(),
                            ?publication,
                            "WAN transit shard demand installed"
                        );
                        self.publish_track_lifecycle(outcome);
                    }
                    if let Some((input, expires)) = self.links.as_ref().and_then(|links| {
                        links.wan_transit_input(network, source, wan_transit::SHARD, now)
                    }) {
                        exports.push(wan_transit::Export {
                            source,
                            endpoint,
                            input,
                            expires: expires.min(*deadline),
                        });
                    }
                }
            }
            if let Some(outbound) = peers
                .get_mut(index)
                .and_then(|peer| peer.association.as_mut())
            {
                let same = |old: &wan_transit::Export, new: &wan_transit::Export| {
                    old.source == new.source
                        && old.input == new.input
                        && old.endpoint == new.endpoint
                };
                let withdrew = outbound
                    .exports
                    .iter()
                    .any(|old| !exports.iter().any(|new| same(old, new)));
                if withdrew {
                    outbound.sender.invalidate();
                }
                outbound.wan_refresh.announce |= withdrew
                    || exports
                        .iter()
                        .any(|new| !outbound.exports.iter().any(|old| same(old, new)));
                if outbound.exports.len() != exports.len() {
                    tracing::debug!(
                        network = outbound.network.epoch(),
                        count = exports.len(),
                        "WAN transit exports changed"
                    );
                }
                outbound.exports = exports;
            }
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
