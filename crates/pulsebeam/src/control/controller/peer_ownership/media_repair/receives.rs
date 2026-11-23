use std::{num::NonZeroU64, task::Poll};

use pulsebeam_link::{LinkTime, PeerControl};
use pulsebeam_runtime::time::{Duration, Instant};

use crate::{
    control::controller::ControllerActor,
    route::fabric::network_repair::{MediaInterest, MediaRepairChild},
};

use super::{Demand, Ready};

const PENDING_LIMIT: usize = 32;
const ATTEMPT_LIMIT: u8 = 2;
// Ready crosses the paced peer loop before the sender can look up the packet,
// install its WAN window, and hand the repair back to the shard for sending.
const RESPONSE_WINDOW: Duration = super::super::transfer::RETRY.saturating_mul(5);

#[derive(Default)]
pub(in super::super) struct RepairReceives {
    pending: Vec<Request>,
    last_request: u64,
}

struct Request {
    interest: MediaInterest,
    attempts: u8,
    current: Option<Attempt>,
}

struct Attempt {
    child: MediaRepairChild,
    demand: Demand,
    first_ready: Option<Instant>,
}

impl RepairReceives {
    pub(in super::super) fn enqueue(&mut self, interest: MediaInterest, now: Instant) -> bool {
        if now >= interest.expires
            || self.pending.len() >= PENDING_LIMIT
            || self.pending.iter().any(|request| {
                request.interest.parent == interest.parent
                    && request.interest.position == interest.position
            })
        {
            return false;
        }
        self.pending.push(Request {
            interest,
            attempts: 0,
            current: None,
        });
        true
    }

    pub(in super::super) fn flush(
        &mut self,
        actor: &mut ControllerActor,
        lane: &PeerControl,
        origin: Instant,
        now: Instant,
        frame_deadline: LinkTime,
    ) {
        if let Some(links) = &mut actor.links {
            links.reap();
        }
        let last_request = &mut self.last_request;
        self.pending.retain_mut(|request| {
            if now >= request.interest.expires {
                request.retire(actor);
                return false;
            }
            if request.current.is_none() && !request.allocate(actor, last_request, now) {
                return false;
            }
            let Some(attempt) = &mut request.current else {
                return false;
            };
            let acknowledged = match actor
                .links
                .as_ref()
                .map(|links| links.wan_media_repair_receive_state(attempt.child, now))
            {
                Some(Poll::Pending) => return true,
                Some(Poll::Ready(Some(window))) if window == attempt.child.window => window,
                _ => {
                    request.retire(actor);
                    return false;
                }
            };
            if attempt
                .first_ready
                .and_then(|sent| sent.checked_add(RESPONSE_WINDOW))
                .is_some_and(|deadline| now >= deadline)
            {
                request.retire(actor);
                return request.attempts < ATTEMPT_LIMIT
                    && request.allocate(actor, last_request, now);
            }
            let Some(parent) = attempt.child.parent.window else {
                request.retire(actor);
                return false;
            };
            let Some(link_deadline) = attempt
                .child
                .expires
                .checked_duration_since(origin)
                .and_then(|elapsed| u64::try_from(elapsed.as_nanos()).ok())
                .map(LinkTime)
            else {
                request.retire(actor);
                return false;
            };
            let ready = Ready {
                demand: attempt.demand,
                window: acknowledged,
            };
            let Some(demand_bytes) = attempt.demand.encode() else {
                request.retire(actor);
                return false;
            };
            let Some(ready_bytes) = ready.encode(parent) else {
                request.retire(actor);
                return false;
            };
            let deadline = frame_deadline.min(link_deadline);
            // Repeating the exact pair tolerates control loss and reordering.
            // Only the first accepted Ready starts the attempt timeout.
            if lane.try_send(demand_bytes, deadline).is_ok()
                && lane.try_send(ready_bytes, deadline).is_ok()
            {
                attempt.first_ready.get_or_insert(now);
            }
            true
        });
    }
}

impl Request {
    fn allocate(
        &mut self,
        actor: &mut ControllerActor,
        last_request: &mut u64,
        now: Instant,
    ) -> bool {
        let Some(request) = last_request.checked_add(1).and_then(NonZeroU64::new) else {
            return false;
        };
        let interest = self.interest;
        let Some(live) = interest.parent.window else {
            return false;
        };
        let demand = Demand {
            live: live.ssrc(),
            source: interest.parent.incarnation,
            branch: interest.branch,
            request,
            position: interest.position,
        };
        if !demand.matches_parent(live) {
            return false;
        }
        let Some(child) = actor.stage_wan_media_repair_receive(
            now,
            interest.parent,
            interest.position,
            interest.expires,
        ) else {
            return false;
        };
        *last_request = request.get();
        self.attempts = self.attempts.saturating_add(1);
        self.current = Some(Attempt {
            child,
            demand,
            first_ready: None,
        });
        true
    }

    fn retire(&mut self, actor: &mut ControllerActor) {
        if let Some(attempt) = self.current.take()
            && let Some(links) = &mut actor.links
        {
            links.cancel_wan_media_repair(attempt.child);
        }
    }
}
