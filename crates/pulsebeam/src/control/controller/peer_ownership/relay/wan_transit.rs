use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU32,
    task::Poll,
};

use pulsebeam_link::{BindingCutoff, NetworkId, NodeRouteKey};
use pulsebeam_runtime::{sync::oneshot, time::Instant};

use crate::{
    control::controller::{
        ControllerActor,
        ownership::{observations::EndpointFact, receipt::Authority},
    },
    entity::EndpointKey,
    link::wan_host::{MAX_EPOCH_WINDOWS, PendingTx},
    route::fabric::{CompiledLiveRelay, LiveNetworkChild, LiveNextHop, LiveRelayChange},
};

use super::{fabric::WanLiveRouteAck, wan_source::WanSourceWindow};

pub(in crate::control) const SHARD: crate::id::ShardId = crate::id::ShardId::new(0);

pub(in crate::control) struct Advertisement {
    pub(in crate::control) source: WanSourceWindow,
    pub(in crate::control) endpoint: (EndpointKey, EndpointFact),
    pub(in crate::control) upstream: NetworkId,
    pub(in crate::control) upstream_cut: Option<std::num::NonZeroU64>,
    pub(in crate::control) expires: Instant,
}

impl Advertisement {
    pub(in crate::control) fn same_source(&self, other: &Self) -> bool {
        self.source.same_window(other.source)
            && self.source.origin == other.source.origin
            && self.endpoint == other.endpoint
            && self.upstream == other.upstream
            && self.upstream_cut == other.upstream_cut
    }
}

pub(in crate::control) struct Export {
    pub(in crate::control) source: WanSourceWindow,
    pub(in crate::control) input: LiveNetworkChild,
    pub(in crate::control) expires: Instant,
}

#[derive(Default)]
pub(in crate::control) struct Sends {
    entries: BTreeMap<NonZeroU32, Entry>,
    used: BTreeSet<NonZeroU32>,
}

struct Entry {
    input: LiveNetworkChild,
    ack: WanLiveRouteAck,
    expires: Instant,
    cutoff: BindingCutoff,
    phase: Phase,
}

enum Phase {
    Host(PendingTx),
    Shard(CompiledLiveRelay, oneshot::Receiver<bool>),
    Active(CompiledLiveRelay),
}

impl Sends {
    pub(in crate::control) fn retired(&self, label: NonZeroU32) -> bool {
        self.used.contains(&label) && !self.entries.contains_key(&label)
    }

    pub(in crate::control::controller::peer_ownership) fn media_repair_grant(
        &self,
        peer: NetworkId,
        demand: super::super::media_repair::Demand,
        now: Instant,
    ) -> Option<super::super::media_repair::Grant> {
        use super::super::media_repair::Grant;
        use crate::{link::wan_host::ParentSend, route::fabric::relay::MediaRepairParent};
        let entry = self.entries.get(&demand.live)?;
        let Phase::Active(route) = &entry.phase else {
            return None;
        };
        let window = entry.ack.window;
        if route.next.peer != peer
            || route.input.incarnation != demand.source
            || !demand.matches_parent(window)
            || now >= entry.expires
            || !entry.cutoff.is_live()
            || !route.tx.is_live()
        {
            return None;
        }
        Some(Grant {
            path: MediaRepairParent::Relay(route.clone()),
            parent: ParentSend {
                cutoff: entry.cutoff.clone(),
                window,
            },
            expires: entry.expires,
        })
    }

    pub(in crate::control) fn step(
        &mut self,
        actor: &mut ControllerActor,
        peer: NetworkId,
        exports: &[Export],
        advertised: &[WanSourceWindow],
        remote: Option<(Instant, &[WanLiveRouteAck])>,
        now: Instant,
    ) {
        let mut desired = BTreeMap::new();
        if let Some((deadline, acks)) = remote.filter(|(deadline, _)| now < *deadline)
            && actor.receipt_trust.as_ref().is_some_and(Authority::ready)
        {
            for &ack in acks {
                if let Some(export) = exports.iter().find(|export| {
                    now < export.expires
                        && export.source.matches_ack(ack)
                        && advertised
                            .iter()
                            .any(|source| source.same_window(export.source))
                }) {
                    desired.insert(
                        ack.window.ssrc(),
                        (export.input, ack, deadline.min(export.expires)),
                    );
                }
            }
        }
        self.reconcile(actor, peer, desired, now, false);
    }

    pub(in crate::control) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(in crate::control) fn reconcile(
        &mut self,
        actor: &mut ControllerActor,
        peer: NetworkId,
        mut desired: BTreeMap<NonZeroU32, (LiveNetworkChild, WanLiveRouteAck, Instant)>,
        now: Instant,
        routed: bool,
    ) {
        self.entries.retain(|ssrc, entry| {
            let keep = desired.remove(ssrc).is_some_and(|(input, ack, expires)| {
                entry.input == input
                    && entry.ack == ack
                    && now < entry.expires.min(expires)
                    && entry.cutoff.is_live()
                    && entry.advance(actor, peer)
                    && (!routed || entry.renew(now, expires))
            });
            if !keep {
                entry.retire(actor);
            }
            keep
        });
        let Some(links) = &mut actor.links else {
            return;
        };
        for (ssrc, (input, ack, expires)) in desired {
            if self.used.len() >= MAX_EPOCH_WINDOWS {
                break;
            }
            if now >= expires || self.used.contains(&ssrc) {
                continue;
            }
            if let Ok((cutoff, pending)) =
                links.stage_wan_relay_transport(peer, input, ack, expires, routed, now)
            {
                self.used.insert(ssrc);
                self.entries.insert(
                    ssrc,
                    Entry {
                        input,
                        ack,
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
            Phase::Shard(route, _) | Phase::Active(route) => {
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
                    let route = CompiledLiveRelay {
                        input: self.input,
                        next: LiveNextHop {
                            peer,
                            key: NodeRouteKey::from_bytes(
                                self.ack.window.ssrc().get().to_be_bytes(),
                            ),
                        },
                        tx,
                    };
                    let Some(receipt) =
                        actor.stage_fabric_relay_change(LiveRelayChange::Install(route.clone()))
                    else {
                        return false;
                    };
                    self.phase = Phase::Shard(route, receipt);
                    true
                }
            },
            Phase::Shard(route, receipt) => match receipt.try_recv() {
                Ok(true) => {
                    self.phase = Phase::Active(route.clone());
                    true
                }
                Err(oneshot::error::TryRecvError::Empty) => true,
                _ => false,
            },
            Phase::Active(route) => route.tx.is_live(),
        }
    }

    fn retire(&self, actor: &mut ControllerActor) {
        self.cutoff.fence();
        if let Some(links) = &mut actor.links {
            links.retire_wan_send(&self.cutoff);
        }
        if let Phase::Shard(route, _) | Phase::Active(route) = &self.phase {
            let _ = actor.stage_fabric_relay_change(LiveRelayChange::Retire(route.clone()));
        }
    }
}
