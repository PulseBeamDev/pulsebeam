use std::{
    collections::{BTreeMap, BTreeSet},
    num::{NonZeroU32, NonZeroU64},
    task::Poll,
};

use pulsebeam_link::{BindingCutoff, NetworkId, NodeRouteKey};
use pulsebeam_runtime::{sync::oneshot, time::Instant};

use crate::{
    control::controller::{
        ControllerActor, ownership::receipt::Authority, peer_ownership::fabric_data,
    },
    id::ShardId,
    link::wan_host::{MAX_EPOCH_WINDOWS, ParentSend, PendingTx},
    route::fabric::{
        CompiledPublisherSend, LiveNextHop, PublisherExpectation, PublisherObservation,
        PublisherSendChange,
        hop_expectations::Path,
        relay::{PeerRepairLookup, RepairLookup},
    },
};

use super::{fabric::WanLiveRouteAck, wan_source::WanSourceWindow};

#[derive(Default)]
pub(in crate::control) struct PublisherSends {
    entries: BTreeMap<NonZeroU32, Entry>,
    used: BTreeSet<NonZeroU32>,
}

#[derive(Clone, Copy)]
struct Permit {
    source: WanSourceWindow,
    ack: WanLiveRouteAck,
    shard: ShardId,
    protected_wire_bps: Option<NonZeroU64>,
}

impl Permit {
    fn same_route(self, other: Self) -> bool {
        self.shard == other.shard
            && self.ack == other.ack
            && self.protected_wire_bps == other.protected_wire_bps
            && self.source.same_window(other.source)
    }
}

struct Entry {
    permit: Permit,
    expires: Instant,
    cutoff: BindingCutoff,
    phase: Phase,
}

enum Phase {
    Host(PendingTx),
    Installing {
        route: CompiledPublisherSend,
        receipt: oneshot::Receiver<bool>,
    },
    Installed(CompiledPublisherSend),
}

impl PublisherSends {
    pub(in crate::control) fn retired(&self, label: NonZeroU32) -> bool {
        self.used.contains(&label) && !self.entries.contains_key(&label)
    }

    pub(in crate::control::controller) fn media_repair_grant(
        &self,
        peer: NetworkId,
        demand: super::super::media_repair::Demand,
        now: Instant,
    ) -> Option<super::super::media_repair::Grant> {
        let grant = self.live_media_grant(peer, demand.live, demand.source, now)?;
        demand.matches_parent(grant.parent.window).then_some(grant)
    }

    pub(in crate::control::controller) fn live_media_grant(
        &self,
        peer: NetworkId,
        label: NonZeroU32,
        incarnation: std::num::NonZeroU64,
        now: Instant,
    ) -> Option<super::super::media_repair::Grant> {
        use super::super::media_repair::{Grant, Source};
        use crate::route::fabric::relay::MediaRepairParent;
        let entry = self.entries.get(&label)?;
        let Phase::Installed(route) = &entry.phase else {
            return None;
        };
        let window = entry.permit.ack.window;
        if entry.permit.protected_wire_bps.is_some()
            || route.next.peer != peer
            || route.publisher.incarnation != incarnation
            || window.profile() == pulsebeam_core::fabric::Profile::Data
            || now >= entry.expires
            || !entry.cutoff.is_live()
            || !route.tx.is_live()
        {
            return None;
        }
        Some(Grant {
            path: Source::Shard(Box::new(MediaRepairParent::Publisher(route.clone()))),
            parent: ParentSend {
                cutoff: entry.cutoff.clone(),
                window,
            },
            expires: entry.expires,
        })
    }

    fn active_data_binding(
        &self,
        peer: NetworkId,
        scope: fabric_data::Scope,
        now: Instant,
    ) -> Option<(&Entry, &CompiledPublisherSend)> {
        let label = NonZeroU32::new(u32::from_be_bytes(scope.key.to_bytes()))?;
        let entry = self.entries.get(&label)?;
        let Phase::Installed(route) = &entry.phase else {
            return None;
        };
        (now < entry.expires
            && entry.cutoff.is_live()
            && route.next.peer == peer
            && route.publisher.incarnation == scope.source
            && route.publisher.emission.profile() == pulsebeam_core::fabric::Profile::Data)
            .then_some((entry, route))
    }

    pub(in crate::control) fn repair_lookup(
        &self,
        peer: NetworkId,
        frame: fabric_data::RepairFrame,
        now: Instant,
    ) -> Option<PeerRepairLookup> {
        let (_, route) = self.active_data_binding(peer, frame.scope, now)?;
        matches!(frame.action, fabric_data::RepairAction::Lookup).then(|| {
            PeerRepairLookup::Publisher(RepairLookup {
                route: route.clone(),
                branch: frame.branch,
                request: frame.request,
            })
        })
    }

    pub(in crate::control) fn repair_parent(
        &self,
        peer: NetworkId,
        scope: fabric_data::Scope,
        now: Instant,
    ) -> Option<(&CompiledPublisherSend, ParentSend)> {
        let (entry, route) = self.active_data_binding(peer, scope, now)?;
        Some((
            route,
            ParentSend {
                cutoff: entry.cutoff.clone(),
                window: entry.permit.ack.window,
            },
        ))
    }

    pub(in crate::control) fn allows_data_expectation(
        &self,
        notice: &PublisherExpectation,
        now: Instant,
    ) -> bool {
        let route = &notice.route;
        now < notice.expires
            && self
                .active_data_binding(
                    route.next.peer,
                    fabric_data::Scope {
                        key: route.next.key,
                        source: route.publisher.incarnation,
                    },
                    now,
                )
                .is_some_and(|(_, current)| current.matches(route))
    }

    pub(in crate::control) fn data_observation(
        &self,
        peer: NetworkId,
        frame: fabric_data::Frame,
        now: Instant,
    ) -> Option<PublisherObservation> {
        let fabric_data::Action::Observed(outcome) = frame.action else {
            return None;
        };
        let (_, route) = self.active_data_binding(peer, frame.scope, now)?;
        Some(PublisherObservation {
            route: route.clone(),
            unit: frame.unit,
            outcome,
        })
    }

    pub(in crate::control) fn step(
        &mut self,
        actor: &mut ControllerActor,
        peer: NetworkId,
        sources: &[WanSourceWindow],
        remote: Option<(Instant, &[WanLiveRouteAck])>,
        now: Instant,
    ) {
        let (expires, acknowledged) = remote
            .filter(|(expires, _)| now < *expires)
            .filter(|_| actor.receipt_trust.as_ref().is_some_and(Authority::ready))
            .unwrap_or((now, &[]));
        self.reconcile(
            actor,
            peer,
            acknowledged.iter().filter_map(|&ack| {
                let &source = sources.iter().find(|source| source.matches_ack(ack))?;
                Some((source, ack, expires, None))
            }),
            now,
        );
    }

    pub(in crate::control) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(in crate::control) fn reconcile(
        &mut self,
        actor: &mut ControllerActor,
        peer: NetworkId,
        permits: impl IntoIterator<
            Item = (
                WanSourceWindow,
                WanLiveRouteAck,
                Instant,
                Option<NonZeroU64>,
            ),
        >,
        now: Instant,
    ) {
        let mut desired = BTreeMap::new();
        for (source, ack, expires, protected_wire_bps) in permits {
            if now >= expires || !source.matches_ack(ack) {
                continue;
            }
            let Some((shard, publisher)) = actor.lifecycle.network_publisher_anchor(
                source.room,
                source.publisher.publication,
                &actor.core.registry,
            ) else {
                continue;
            };
            if publisher == source.publisher {
                desired.insert(
                    ack.window.ssrc(),
                    (
                        Permit {
                            source,
                            ack,
                            shard,
                            protected_wire_bps,
                        },
                        expires,
                    ),
                );
            }
        }
        self.entries.retain(|ssrc, entry| {
            let retained = desired.remove(ssrc).is_some_and(|(permit, expires)| {
                permit.same_route(entry.permit)
                    && now < expires.min(entry.expires)
                    && entry.cutoff.is_live()
                    && entry.advance(actor, peer)
                    && entry.renew(now, expires)
            });
            if !retained {
                entry.retire(actor);
            }
            retained
        });
        let Some(links) = &mut actor.links else {
            return;
        };
        for (ssrc, (permit, expires)) in desired {
            if self.used.len() >= MAX_EPOCH_WINDOWS {
                break;
            }
            if self.used.contains(&ssrc) {
                continue;
            }
            if let Ok((cutoff, pending)) = links.stage_wan_publisher_transport(
                peer,
                permit.source,
                permit.ack,
                expires,
                now,
                permit.protected_wire_bps,
            ) {
                self.used.insert(ssrc);
                self.entries.insert(
                    ssrc,
                    Entry {
                        permit,
                        expires,
                        cutoff,
                        phase: Phase::Host(pending),
                    },
                );
            }
        }
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.cutoff.fence();
    }
}

impl Entry {
    fn renew(&mut self, now: Instant, expires: Instant) -> bool {
        match &self.phase {
            Phase::Host(_) => return expires >= self.expires,
            Phase::Installing { route, .. } | Phase::Installed(route) => {
                if !route.tx.renew(now, expires) {
                    return false;
                }
            }
        }
        self.expires = expires;
        true
    }

    fn advance(&mut self, actor: &mut ControllerActor, peer: NetworkId) -> bool {
        match &mut self.phase {
            Phase::Host(pending) => match pending.poll() {
                Poll::Pending => true,
                Poll::Ready(None) => false,
                Poll::Ready(Some(tx)) => {
                    let route = CompiledPublisherSend {
                        publisher: self.permit.source.publisher,
                        shard: self.permit.shard,
                        next: LiveNextHop {
                            peer,
                            key: NodeRouteKey::from_bytes(
                                self.permit.ack.window.ssrc().get().to_be_bytes(),
                            ),
                        },
                        tx,
                    };
                    let change = PublisherSendChange::Install {
                        route: route.clone(),
                        lifetime: self.cutoff.clone(),
                    };
                    let Some(receipt) = actor.stage_publisher_send(change) else {
                        return false;
                    };
                    self.phase = Phase::Installing { route, receipt };
                    true
                }
            },
            Phase::Installing { route, receipt } => match receipt.try_recv() {
                Err(oneshot::error::TryRecvError::Empty) => true,
                Ok(true) => {
                    self.phase = Phase::Installed(route.clone());
                    true
                }
                _ => false,
            },
            Phase::Installed(_) => true,
        }
    }

    fn retire(&self, actor: &mut ControllerActor) {
        self.cutoff.fence();
        if let Some(links) = &mut actor.links {
            links.retire_wan_send(&self.cutoff);
        }
        if let Phase::Installed(route) | Phase::Installing { route, .. } = &self.phase {
            let _ = actor.stage_publisher_send(PublisherSendChange::Retire(route.clone()));
        }
    }
}
