use std::{collections::BTreeMap, num::NonZeroU64};

use pulsebeam_runtime::time::{Duration, Instant};

const DEMAND_LIMIT: usize = 32;
const RETRY: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RepairFragment {
    pub(crate) unit_seq: u64,
    pub(crate) index: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RepairRequest {
    pub(crate) fragment: RepairFragment,
    pub(crate) id: NonZeroU64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RepairAdmission {
    First,
    Coalesced,
    Replay,
    Exhausted,
    Ineligible,
}

#[derive(Clone, Copy)]
struct Pending {
    id: NonZeroU64,
    expires: Instant,
    next: Instant,
    queued: Option<u64>,
    network_installed: bool,
}

pub(super) struct Repairs {
    pub(super) incarnation: NonZeroU64,
    last: Option<NonZeroU64>,
    demands: BTreeMap<RepairFragment, Pending>,
}

impl Repairs {
    pub(super) fn new(incarnation: NonZeroU64) -> Self {
        Self {
            incarnation,
            last: None,
            demands: BTreeMap::new(),
        }
    }

    pub(super) fn request(
        &mut self,
        now: Instant,
        request: RepairRequest,
        until: Instant,
    ) -> RepairAdmission {
        if self.last.is_some_and(|last| request.id <= last) {
            return RepairAdmission::Replay;
        }
        self.last = Some(request.id);
        if self.demands.contains_key(&request.fragment) {
            return RepairAdmission::Coalesced;
        }
        if self.demands.len() >= DEMAND_LIMIT {
            return RepairAdmission::Exhausted;
        }
        self.demands.insert(
            request.fragment,
            Pending {
                id: request.id,
                expires: until,
                next: now,
                queued: None,
                network_installed: false,
            },
        );
        RepairAdmission::First
    }

    pub(super) fn cancel(&mut self, request: RepairRequest) -> bool {
        if !self
            .demands
            .get(&request.fragment)
            .is_some_and(|pending| pending.id == request.id)
        {
            return false;
        }
        self.demands.remove(&request.fragment);
        true
    }

    pub(super) fn expire(&mut self, now: Instant) -> Option<RepairRequest> {
        let (&fragment, pending) = self
            .demands
            .iter()
            .find(|(_, pending)| now >= pending.expires)?;
        let id = pending.id;
        self.demands.remove(&fragment);
        Some(RepairRequest { fragment, id })
    }

    pub(super) fn next_expiry(&self) -> Option<(Instant, RepairRequest)> {
        self.demands
            .iter()
            .map(|(&fragment, pending)| {
                (
                    pending.expires,
                    RepairRequest {
                        fragment,
                        id: pending.id,
                    },
                )
            })
            .min_by_key(|(expires, request)| (*expires, request.id))
    }

    pub(super) fn next(&self) -> Option<(Instant, Instant, RepairRequest, bool)> {
        self.demands
            .iter()
            .map(|(&fragment, pending)| {
                (
                    pending.next,
                    pending.expires,
                    RepairRequest {
                        fragment,
                        id: pending.id,
                    },
                    pending.queued.is_some(),
                )
            })
            .min_by_key(|(next, _, request, _)| (*next, request.id))
    }

    pub(super) fn defer(&mut self, now: Instant, request: RepairRequest) {
        if let Some(pending) = self
            .demands
            .get_mut(&request.fragment)
            .filter(|pending| pending.id == request.id)
        {
            pending.next = now
                .checked_add(RETRY)
                .unwrap_or(pending.expires)
                .min(pending.expires);
        }
    }

    pub(super) fn arm_network_repair(&mut self, request: RepairRequest) -> bool {
        let Some(pending) = self
            .demands
            .get_mut(&request.fragment)
            .filter(|pending| pending.id == request.id)
        else {
            return false;
        };
        if pending.network_installed {
            return false;
        }
        pending.network_installed = true;
        true
    }

    pub(super) fn is_queued(&self, request: RepairRequest) -> bool {
        self.demands
            .get(&request.fragment)
            .is_some_and(|pending| pending.id == request.id && pending.queued.is_some())
    }

    pub(super) fn pending_until(&self, request: RepairRequest) -> Option<Instant> {
        let pending = self.demands.get(&request.fragment)?;
        (pending.id == request.id).then_some(pending.expires)
    }

    pub(super) fn for_fragment(&self, fragment: RepairFragment) -> Option<RepairRequest> {
        let pending = self.demands.get(&fragment)?;
        Some(RepairRequest {
            fragment,
            id: pending.id,
        })
    }

    pub(super) fn can_claim(&self, now: Instant, request: RepairRequest, sequence: u64) -> bool {
        self.demands.get(&request.fragment).is_some_and(|pending| {
            pending.id == request.id && now < pending.expires && pending.queued.is_none()
        }) && self.queued(now, sequence).is_none()
    }

    pub(super) fn queued(&self, now: Instant, sequence: u64) -> Option<RepairRequest> {
        self.demands.iter().find_map(|(&fragment, pending)| {
            (pending.queued == Some(sequence) && now < pending.expires).then_some(RepairRequest {
                fragment,
                id: pending.id,
            })
        })
    }

    pub(super) fn release(&mut self, sequence: u64) {
        if let Some(pending) = self
            .demands
            .values_mut()
            .find(|pending| pending.queued == Some(sequence))
        {
            pending.queued = None;
        }
    }

    /// Record the association only after a successful enqueue, or when attaching
    /// to a live send already in the branch queue.
    pub(super) fn claim(&mut self, now: Instant, request: RepairRequest, sequence: u64) -> bool {
        if !self.can_claim(now, request, sequence) {
            return false;
        }
        let Some(pending) = self.demands.get_mut(&request.fragment) else {
            return false;
        };
        pending.queued = Some(sequence);
        true
    }

    pub(super) fn handoff(
        &mut self,
        now: Instant,
        request: RepairRequest,
        sequence: u64,
        write: impl FnOnce() -> bool,
    ) -> bool {
        if !self.demands.get(&request.fragment).is_some_and(|pending| {
            pending.id == request.id && now < pending.expires && pending.queued == Some(sequence)
        }) || !write()
        {
            return false;
        }
        self.demands.remove(&request.fragment);
        true
    }
}
