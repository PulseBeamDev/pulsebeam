use pulsebeam_runtime::time::Instant;

use super::local::{MAX_BRANCHES, Offers, Transit};
use super::{CompiledLiveRelay, LiveAdmissions, LiveNetworkChild, LiveNextHop};
use crate::{link::wan_host::WanTx, route::NodeRouteAddress};

/// Branch admission belongs to the downstream key, not to a particular input path.
#[derive(Default)]
pub(crate) struct LiveRelays {
    branches: Vec<Branch>,
}

pub(crate) fn offer_live_network(
    admissions: &mut LiveAdmissions,
    tx: &WanTx,
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

#[derive(Clone, Debug)]
pub(crate) struct RepairLookup<R> {
    pub(crate) route: R,
    pub(crate) branch: std::num::NonZeroU64,
    pub(crate) request: super::local::RepairRequest,
}

pub(crate) type PublisherRepairLookup = RepairLookup<super::CompiledPublisherSend>;

#[derive(Debug)]
pub(crate) enum PeerRepairLookup {
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
    pub(crate) tx: WanTx,
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

    pub(crate) fn retire(&mut self, relay: &CompiledLiveRelay) {
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
