use pulsebeam_runtime::time::Instant;

use super::hop_expectations::{HopExpectations, Notice, Path, Step};

use super::local::{DataExpectation, MAX_BRANCHES, Offers, PropagationOutcome, Transit};
use super::{
    CompiledLiveRelay, LiveAdmissions, LiveNetworkChild, LiveNextHop, NetworkDataExpectation,
};
use crate::{link::fabric_tx::FabricTx, route::NodeRouteAddress};

/// Branch admission belongs to the downstream key, not to a particular input path.
#[derive(Default)]
pub(crate) struct LiveRelays {
    branches: Vec<Branch>,
    expectations: HopExpectations<LiveNetworkChild, CompiledLiveRelay>,
}

pub(crate) fn offer_live_network(
    admissions: &mut LiveAdmissions,
    tx: &FabricTx,
    expires_at: Instant,
    transit: &Transit<'_>,
    now: Instant,
) -> Option<super::LiveOffer> {
    let sequence = transit.work.sequence();
    let deadline = expires_at.min(transit.work.expires_at());
    if deadline <= now || transit.packet.remaining_hops() == 0 || !tx.covers(sequence) {
        return None;
    }
    Some(transit.work.enqueue(now, admissions, || {
        tx.try_send(now, deadline, sequence, transit.packet.clone().into_owned())
    }))
}

impl Path for CompiledLiveRelay {
    fn matches(&self, other: &Self) -> bool {
        self.next == other.next && self.input == other.input && self.tx == other.tx
    }

    fn deadline(&self) -> Instant {
        self.expires_at
    }
}

pub(crate) type RelayExpectation = Notice<CompiledLiveRelay>;
pub(crate) type RelayExpectationStep = Step<CompiledLiveRelay>;

#[derive(Debug)]
pub(crate) struct RelayObservation {
    pub(crate) route: CompiledLiveRelay,
    pub(crate) unit: DataExpectation,
    pub(crate) outcome: PropagationOutcome,
}

#[derive(Clone, Debug)]
pub(crate) struct RepairLookup<R> {
    pub(crate) route: R,
    pub(crate) branch: std::num::NonZeroU64,
    pub(crate) request: super::local::RepairRequest,
}

pub(crate) type RelayRepairLookup = RepairLookup<CompiledLiveRelay>;
pub(crate) type PublisherRepairLookup = RepairLookup<super::CompiledPublisherSend>;

#[derive(Debug)]
pub(crate) enum PeerRepairLookup {
    Relay(RelayRepairLookup),
    Publisher(PublisherRepairLookup),
    Media(MediaRepairLookup),
}

#[derive(Clone, Debug)]
pub(crate) struct MediaRepairLookup {
    pub(crate) parent: MediaRepairParent,
    pub(crate) position: u64,
}

#[derive(Clone, Debug)]
pub(crate) enum MediaRepairParent {
    Relay(CompiledLiveRelay),
    Publisher(super::CompiledPublisherSend),
}

impl MediaRepairParent {
    pub(crate) fn shard(&self) -> crate::id::ShardId {
        match self {
            Self::Relay(route) => route.input.destination.shard(),
            Self::Publisher(route) => route.shard,
        }
    }
}

impl PeerRepairLookup {
    pub(crate) fn shard(&self) -> crate::id::ShardId {
        match self {
            Self::Relay(lookup) => lookup.route.input.destination.shard(),
            Self::Publisher(lookup) => lookup.route.shard,
            Self::Media(lookup) => lookup.parent.shard(),
        }
    }
}

#[derive(Debug)]
pub(crate) struct LocatedRepair {
    pub(crate) lookup: Box<PeerRepairLookup>,
    pub(crate) sequence: u64,
    pub(crate) expires: Instant,
}

#[derive(Debug)]
pub(crate) struct PeerRepairHandoff {
    pub(crate) located: LocatedRepair,
    pub(crate) tx: FabricTx,
}

struct Branch {
    next: LiveNextHop,
    paths: Vec<CompiledLiveRelay>,
    admissions: LiveAdmissions,
}

impl LiveRelays {
    pub(crate) fn install(&mut self, now: Instant, relay: CompiledLiveRelay) -> bool {
        if relay.expires_at <= now || !relay.tx.is_live() {
            return false;
        }
        let existing = self
            .branches
            .iter()
            .position(|branch| branch.next == relay.next);
        for branch in &mut self.branches {
            branch.paths.retain(|path| path.expires_at > now);
        }
        if let Some(index) = existing
            && let Some(branch) = self.branches.get_mut(index)
            && let Some(path) = branch
                .paths
                .iter_mut()
                .find(|path| path.input == relay.input)
        {
            *path = relay;
            return true;
        }
        let paths = self.branches.iter().fold(0_usize, |sum, branch| {
            sum.saturating_add(branch.paths.len())
        });
        if paths >= MAX_BRANCHES {
            return false;
        }
        if let Some(index) = existing {
            let Some(branch) = self.branches.get_mut(index) else {
                return false;
            };
            branch.paths.push(relay);
        } else {
            if self.branches.len() >= MAX_BRANCHES {
                return false;
            }
            self.branches.push(Branch {
                next: relay.next,
                paths: vec![relay],
                admissions: LiveAdmissions::default(),
            });
        }
        true
    }

    pub(crate) fn retire(
        &mut self,
        relay: &CompiledLiveRelay,
        retirement: super::RelayRetirement,
        now: Instant,
    ) {
        self.expectations.retire(relay, retirement, now);
        if let Some(branch) = self
            .branches
            .iter_mut()
            .find(|branch| branch.next == relay.next)
        {
            branch
                .paths
                .retain(|path| path.input != relay.input || path.tx != relay.tx);
        }
    }

    pub(crate) fn retire_input(&mut self, address: NodeRouteAddress) {
        for branch in &mut self.branches {
            branch
                .paths
                .retain(|path| path.input.destination != address);
        }
    }

    /// Called only on first expectation acceptance by the owning fanout.
    pub(crate) fn seed_expectation(
        &mut self,
        now: Instant,
        notice: NetworkDataExpectation,
    ) -> bool {
        let routes = self.branches.iter().filter_map(|branch| {
            branch.paths.iter().find(|path| {
                path.input == notice.parent && path.expires_at > now && path.tx.is_live()
            })
        });
        self.expectations
            .seed(now, notice.parent, notice.unit, notice.expires, routes)
    }

    pub(crate) fn has_expectation(&self, notice: NetworkDataExpectation, now: Instant) -> bool {
        self.expectations.contains(notice.parent, notice.unit, now)
    }

    pub(crate) fn repair_parent(
        &self,
        fragment: super::local::RepairFragment,
        now: Instant,
    ) -> Option<LiveNetworkChild> {
        self.expectations.parent(fragment, now)
    }

    pub(crate) fn expectation_deadline(&self) -> Option<Instant> {
        self.expectations.deadline()
    }

    fn exact_path(branches: &[Branch], route: &CompiledLiveRelay, now: Instant) -> bool {
        route.tx.is_live()
            && branches.iter().any(|branch| {
                branch.next == route.next
                    && branch.paths.iter().any(|path| {
                        path.input == route.input && path.tx == route.tx && now < path.expires_at
                    })
            })
    }

    pub(crate) fn has_live_path(&self, route: &CompiledLiveRelay, now: Instant) -> bool {
        Self::exact_path(&self.branches, route, now)
    }

    pub(crate) fn tick_expectation(&mut self, now: Instant) -> Option<RelayExpectationStep> {
        let branches = &self.branches;
        self.expectations
            .tick(now, |route| Self::exact_path(branches, route, now))
    }

    pub(crate) fn observe_expectation(
        &mut self,
        now: Instant,
        route: &CompiledLiveRelay,
        unit: DataExpectation,
        outcome: PropagationOutcome,
    ) -> bool {
        Self::exact_path(&self.branches, route, now)
            && self.expectations.observe(now, route, unit, outcome)
    }

    pub(crate) fn repair_window(
        &self,
        route: &CompiledLiveRelay,
        fragment: super::local::RepairFragment,
        now: Instant,
    ) -> Option<(Instant, u16)> {
        if !Self::exact_path(&self.branches, route, now) {
            return None;
        }
        self.expectations.confirmed_window(route, fragment, now)
    }

    pub(crate) fn offer(
        &mut self,
        now: Instant,
        input: LiveNetworkChild,
        frozen: Transit<'_>,
        report: &mut Offers,
    ) {
        if frozen.packet.remaining_hops() == 0 {
            return;
        }
        for branch in &mut self.branches {
            let Some(path) = branch.paths.iter().find(|path| path.input == input) else {
                continue;
            };
            if let Some(outcome) = offer_live_network(
                &mut branch.admissions,
                &path.tx,
                path.expires_at,
                &frozen,
                now,
            ) {
                report.record(outcome);
            }
        }
    }
}
