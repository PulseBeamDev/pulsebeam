use std::task::Poll;

use pulsebeam_link::{BindingCutoff, BindingId, NetworkId};
use pulsebeam_runtime::time::Instant;

use crate::{
    control::{controller::ControllerActor, link::wan::RepairSendPermit},
    link::{
        fabric_tx::FabricTx,
        wan_host::{ParentSend, PendingTx},
    },
    route::fabric::relay::{LocatedRepair, PeerRepairHandoff},
};

#[derive(Default)]
pub(in crate::control::controller::peer_ownership) struct RepairSends {
    flights: Vec<Flight>,
}

struct Flight {
    parent: BindingCutoff,
    cutoff: BindingCutoff,
    expires: Instant,
    staged: Option<Box<StagedRepair>>,
}

struct StagedRepair {
    pending: PendingTx,
    located: LocatedRepair,
}

impl Drop for Flight {
    fn drop(&mut self) {
        self.cutoff.fence();
    }
}

impl RepairSends {
    pub(in crate::control::controller::peer_ownership) fn start(
        &mut self,
        actor: &mut ControllerActor,
        peer: NetworkId,
        parent: ParentSend,
        permit: impl Into<RepairSendPermit>,
        located: LocatedRepair,
        now: Instant,
    ) -> Option<BindingId> {
        if self.flights.len() >= super::super::provisioned::REPAIR_LOOKUP_BUDGET {
            return None;
        }
        let expires = located.expires;
        let (cutoff, pending) = actor
            .links
            .as_mut()?
            .stage_wan_repair_transport(peer, parent.clone(), permit, expires, now)
            .ok()?;
        let binding = cutoff.id();
        self.flights.push(Flight {
            parent: parent.cutoff,
            cutoff,
            expires,
            staged: Some(Box::new(StagedRepair { pending, located })),
        });
        Some(binding)
    }

    pub(in crate::control::controller::peer_ownership) fn step(
        &mut self,
        actor: &mut ControllerActor,
        now: Instant,
    ) {
        self.flights.retain_mut(|flight| {
            if now < flight.expires
                && flight.parent.is_live()
                && flight.cutoff.is_live()
                && flight.advance(actor)
            {
                return true;
            }
            flight.retire(actor);
            false
        });
    }

    pub(in crate::control::controller::peer_ownership) fn cancel(
        &mut self,
        actor: &mut ControllerActor,
        binding: BindingId,
    ) {
        if let Some(index) = self
            .flights
            .iter()
            .position(|flight| flight.cutoff.id() == binding)
        {
            self.flights.swap_remove(index).retire(actor);
        }
    }
}

impl Flight {
    fn advance(&mut self, actor: &mut ControllerActor) -> bool {
        let Some(mut staged) = self.staged.take() else {
            return true;
        };
        match staged.pending.poll() {
            Poll::Pending => {
                self.staged = Some(staged);
                true
            }
            Poll::Ready(Some(tx)) => actor.stage_wan_repair_handoff(PeerRepairHandoff {
                located: staged.located,
                tx: FabricTx::Wan(tx),
            }),
            Poll::Ready(None) => false,
        }
    }

    fn retire(&self, actor: &mut ControllerActor) {
        self.cutoff.fence();
        if let Some(links) = &mut actor.links {
            links.retire_wan_send(&self.cutoff);
        }
    }
}
