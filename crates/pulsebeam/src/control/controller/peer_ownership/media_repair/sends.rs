use std::{
    collections::BTreeMap,
    num::{NonZeroU32, NonZeroU64},
};

use pulsebeam_link::BindingCutoff;
use pulsebeam_runtime::{sync::oneshot, time::Instant};

use crate::{
    link::wan_host::ParentSend,
    route::fabric::{
        network_repair::MAX_REPAIR_LIFETIME,
        relay::{LocatedRepair, MediaRepairLookup, MediaRepairParent, PeerRepairLookup},
    },
};

use super::{Demand, Ready};

const MAX_PENDING: usize = 32;
const MAX_REQUESTS: usize = 4096;

type RequestKey = (NonZeroU32, NonZeroU64, NonZeroU64);
type Receipt = oneshot::Receiver<Option<LocatedRepair>>;

pub(in crate::control::controller) struct Grant {
    pub(in super::super) path: MediaRepairParent,
    pub(in super::super) parent: ParentSend,
    pub(in super::super) expires: Instant,
}

impl Grant {
    pub(in crate::control::controller) fn expires(&self) -> Instant {
        self.expires
    }

    pub(in crate::control::controller) fn apply_target_demand(
        self,
        actor: &mut crate::control::controller::ControllerActor,
        demand: super::super::target_demand::Demand,
        now: Instant,
    ) {
        use crate::route::fabric::{
            LiveRelayChange, PublisherSendChange, relay::MediaRepairParent,
            target_demand::VersionedUpdate,
        };
        if self.parent.window.profile() != pulsebeam_core::fabric::Profile::Vp9 {
            return;
        }
        let Some(update) = demand.update(now, self.expires) else {
            return;
        };
        let update = VersionedUpdate {
            generation: demand.generation,
            update,
        };
        match self.path {
            MediaRepairParent::Publisher(route) => {
                let _ = actor.stage_publisher_send(PublisherSendChange::Demand(route, update));
            }
            MediaRepairParent::Relay(route) => {
                let _ = actor.stage_fabric_relay_change(LiveRelayChange::Demand(route, update));
            }
        }
    }

    pub(in crate::control::controller) fn into_refresh(
        self,
        demand: Demand,
    ) -> Option<PeerRepairLookup> {
        (self.parent.window.profile() == pulsebeam_core::fabric::Profile::Vp9).then_some(
            PeerRepairLookup::Refresh(MediaRepairLookup {
                parent: self.path,
                position: demand.position,
            }),
        )
    }
}

#[derive(Default)]
pub(in super::super) struct RepairRequests {
    pending: Vec<Request>,
    // A request key is burned until its live label is retired. Repair timeout
    // is not permission to reopen the same request with another repair SSRC.
    seen: BTreeMap<RequestKey, BindingCutoff>,
}

struct Request {
    demand: Demand,
    parent: ParentSend,
    expires: Instant,
    locating: Receipt,
    located: Option<LocatedRepair>,
    ready: Option<Ready>,
}

impl RepairRequests {
    pub(in super::super) fn demand(
        &mut self,
        demand: Demand,
        grant: Grant,
        now: Instant,
        locate: impl FnOnce(MediaRepairParent) -> Option<Receipt>,
    ) {
        let key = (demand.live, demand.branch, demand.request);
        if self.pending.len() >= MAX_PENDING
            || self.seen.len() >= MAX_REQUESTS
            || self.seen.contains_key(&key)
            || !grant.parent.cutoff.is_live()
            || !demand.matches_parent(grant.parent.window)
        {
            return;
        }
        let Some(limit) = now.checked_add(MAX_REPAIR_LIFETIME) else {
            return;
        };
        let expires = limit.min(grant.expires);
        if expires <= now {
            return;
        }
        let Some(locating) = locate(grant.path) else {
            return;
        };
        self.seen.insert(key, grant.parent.cutoff.clone());
        self.pending.push(Request {
            demand,
            parent: grant.parent,
            expires,
            locating,
            located: None,
            ready: None,
        });
    }

    pub(in super::super) fn ready(&mut self, bytes: &[u8], demand: Demand, now: Instant) {
        let Some(request) = self.pending.iter_mut().find(|request| {
            request.demand == demand && now < request.expires && request.parent.cutoff.is_live()
        }) else {
            return;
        };
        if request.ready.is_none() {
            request.ready = Ready::decode(bytes, request.parent.window);
        }
    }

    pub(in super::super) fn flush(
        &mut self,
        now: Instant,
        mut handoff: impl FnMut(ParentSend, Ready, LocatedRepair),
    ) {
        self.seen.retain(|_, cutoff| cutoff.is_live());
        self.pending.retain_mut(|request| {
            if now >= request.expires || !request.parent.cutoff.is_live() {
                return false;
            }
            if request.located.is_none() {
                match request.locating.try_recv() {
                    Ok(Some(mut located)) if located.sequence == request.demand.position => {
                        located.expires = located.expires.min(request.expires);
                        request.located = Some(located);
                    }
                    Err(oneshot::error::TryRecvError::Empty) => return true,
                    _ => return false,
                }
            }
            if request
                .located
                .as_ref()
                .is_some_and(|located| now >= located.expires)
            {
                return false;
            }
            if let Some(ready) = request.ready
                && let Some(located) = request.located.take()
            {
                handoff(request.parent.clone(), ready, located);
                return false;
            }
            true
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        entity::{ConnectionId, TrackKind},
        id::ShardId,
        route::fabric::{
            CompiledPublisherSend, LiveNextHop,
            local::Emission,
            relay::{MediaRepairLookup, PeerRepairLookup},
        },
        shard_update::PublisherAnchor,
    };
    use pulsebeam_core::fabric::{Profile, wan::Window};
    use pulsebeam_link::NodeRouteKey;
    use pulsebeam_runtime::time::Duration;

    #[test]
    fn media_ready_requires_a_live_single_use_demand_even_after_timeout() {
        let now = Instant::now();
        let expires = now + Duration::from_secs(5);
        let demand = Demand {
            live: NonZeroU32::MIN,
            source: NonZeroU64::MIN,
            branch: NonZeroU64::MIN,
            request: NonZeroU64::MIN,
            position: 0x18001,
        };
        let live = Window::new(Profile::H264, 0x18000, 0, demand.live, 0).unwrap();
        let (_fixture, peer, tx) = crate::link::wan_host::SendFixture::new(live, expires);
        let cutoff = tx.cutoff();
        let repair = live
            .repair_window(demand.position, NonZeroU32::new(2).unwrap())
            .unwrap();
        let connection = ConnectionId::from_bytes([7; 16]);
        let grant = || Grant {
            path: MediaRepairParent::Publisher(CompiledPublisherSend {
                publisher: PublisherAnchor {
                    publication: connection.fixture_publication(TrackKind::Video, "camera"),
                    connection,
                    incarnation: demand.source,
                    emission: Emission::new(Profile::H264, &[]).unwrap(),
                },
                shard: ShardId::new(0),
                next: LiveNextHop {
                    peer,
                    key: NodeRouteKey::from_bytes(demand.live.get().to_be_bytes()),
                },
                tx: tx.clone(),
            }),
            parent: ParentSend {
                cutoff: cutoff.clone(),
                window: live,
            },
            expires,
        };
        let mut book = RepairRequests::default();
        let wire = |demand| {
            Ready {
                demand,
                window: repair,
            }
            .encode(live)
            .unwrap()
        };
        let ready = wire(demand);
        book.ready(&ready, demand, now);
        let mut located = None;
        book.demand(demand, grant(), now, |parent| {
            let (result, receipt) = oneshot::channel();
            located = Some((
                result,
                LocatedRepair {
                    lookup: Box::new(PeerRepairLookup::Media(MediaRepairLookup {
                        parent,
                        position: demand.position,
                    })),
                    sequence: demand.position,
                    expires,
                },
            ));
            Some(receipt)
        });
        let (result, packet) = located.unwrap();
        result.send(Some(packet)).unwrap();
        book.flush(now, |_, _, _| {
            panic!("unsolicited Ready must not be retained")
        });
        let changed = Demand {
            position: demand.position + 1,
            ..demand
        };
        book.demand(changed, grant(), now, |_| {
            panic!("one request cannot change position")
        });
        book.ready(&wire(changed), changed, now);
        book.flush(now, |_, _, _| panic!("changed position cannot send"));
        book.ready(&ready, demand, now);
        let mut sent = 0;
        book.flush(now, |parent, confirmed, located| {
            assert_eq!(parent.cutoff.id(), cutoff.id());
            assert_eq!(confirmed.demand, demand);
            assert_eq!(located.expires, now + MAX_REPAIR_LIFETIME);
            sent += 1;
        });
        assert_eq!(sent, 1);
        let later = now + MAX_REPAIR_LIFETIME;
        book.ready(&ready, demand, later);
        book.demand(demand, grant(), later, |_| {
            panic!("expiry does not reopen a spent request")
        });
        book.flush(later, |_, _, _| panic!("one request cannot send twice"));
        cutoff.fence();
        let fresh = Demand {
            request: NonZeroU64::new(2).unwrap(),
            ..demand
        };
        book.demand(fresh, grant(), later, |_| {
            panic!("retired parent cannot locate a packet")
        });
    }
}
