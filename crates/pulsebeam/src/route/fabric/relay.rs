use pulsebeam_core::{dd::evidence::ProjectionEvidence, fabric::wan::OmissionRun};
use pulsebeam_runtime::time::Instant;

use super::local::{BasePacket, LiveWork, MAX_BRANCHES, Offers, OwnedPacket, Transit};
use super::target_demand::Projection;
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

#[derive(Default)]
pub(crate) struct BaseProjection {
    through: Option<u64>,
    omitted: u8,
}

impl BaseProjection {
    fn observe(&mut self, position: u64, packet: BasePacket) -> Option<OmissionRun> {
        if self.through.is_some_and(|through| position <= through) {
            return None;
        }
        if self.through.and_then(|through| through.checked_add(1)) != Some(position) {
            self.omitted = 0;
        }
        self.through = Some(position);
        match packet {
            BasePacket::Base => OmissionRun::new(std::mem::take(&mut self.omitted)),
            BasePacket::Enhancement => {
                self.omitted = self.omitted.saturating_add(1).min(OmissionRun::MAX);
                None
            }
            BasePacket::Unknown => {
                self.omitted = 0;
                None
            }
        }
    }

    pub(crate) fn offer(
        &mut self,
        now: Instant,
        tx: &WanTx,
        admissions: &mut LiveAdmissions,
        transit: &Transit<'_>,
        report: &mut Offers,
    ) {
        let position = transit.work.sequence();
        if !tx.covers(position) {
            return;
        }
        let omitted = self.observe(position, transit.h264_base);
        tracing::trace!(target: "pulsebeam_protection", position, timestamp = transit.packet.header().timestamp, packet = ?transit.h264_base, ?omitted, "protected publisher classification");
        if transit.h264_base != BasePacket::Base || transit.packet.remaining_hops() == 0 {
            return;
        }
        let deadline = tx.expires().min(transit.work.expires_at());
        if now >= deadline {
            return;
        }
        #[cfg(feature = "sim")]
        if transit.packet.header().marker {
            crate::sim_metrics::wan_repair::select_protected_base(
                position,
                transit.packet.header().timestamp,
            );
        }
        let omitted =
            omitted.filter(|&run| tx.accommodates_omission(position, &transit.packet, run));
        report.record(transit.work.enqueue(now, admissions, || {
            tx.try_send_projected(
                now,
                deadline,
                position,
                transit.packet.clone().into_owned(),
                omitted,
            )
        }));
    }
}

#[cfg(test)]
#[test]
fn base_projection_only_proves_consecutive_known_enhancements() {
    use BasePacket::{Base, Enhancement, Unknown};
    let mut projection = BaseProjection::default();
    assert_eq!(projection.observe(10, Base), None);
    assert_eq!(projection.observe(11, Enhancement), None);
    assert_eq!(projection.observe(12, Enhancement), None);
    assert_eq!(projection.observe(13, Base), OmissionRun::new(2));
    assert_eq!(projection.observe(14, Unknown), None);
    assert_eq!(projection.observe(14, Enhancement), None);
    assert_eq!(projection.observe(15, Base), None);
    assert_eq!(projection.observe(16, Enhancement), None);
    assert_eq!(projection.observe(18, Base), None);
    assert_eq!(projection.observe(19, Enhancement), None);
    assert_eq!(projection.observe(20, Unknown), None);
    assert_eq!(projection.observe(21, Enhancement), None);
    assert_eq!(projection.observe(22, Base), OmissionRun::new(1));
}

/// One outbound window's tentative exclusions. Keep originals until the
/// carrier can take the proof, so a gap, update or oversized packet can fall
/// back to ordinary forwarding without turning an omission into false loss.
#[derive(Default)]
pub(crate) struct SendProjection {
    through: Option<u64>,
    pending: Vec<PendingExclusion>,
}

struct PendingExclusion {
    work: LiveWork,
    packet: OwnedPacket,
    proof: ProjectionEvidence,
}

fn excludes(requested: Option<Projection>, proof: ProjectionEvidence) -> bool {
    match requested {
        Some(Projection::Targets { context, targets }) => proof.excludes(context, targets),
        _ => false,
    }
}

impl SendProjection {
    pub(crate) fn offer(
        &mut self,
        now: Instant,
        tx: &WanTx,
        admissions: &mut LiveAdmissions,
        requested: Option<Projection>,
        transit: &Transit<'_>,
        report: &mut Offers,
    ) {
        let position = transit.work.sequence();
        let expires = tx.expires();
        if now >= expires || !tx.is_live() || !tx.covers(position) {
            return;
        }
        let fresh = self.through.is_none_or(|previous| position > previous);
        if !fresh
            && self
                .pending
                .iter()
                .any(|pending| pending.work.sequence() == position)
        {
            return;
        }
        let continues = fresh
            && self.through.and_then(|last| last.checked_add(1)) == Some(position)
            && self.pending.iter().all(|pending| {
                now < pending.work.expires_at() && excludes(requested, pending.proof)
            });
        if !continues && fresh {
            self.flush(now, tx, admissions, report);
        }
        if fresh {
            self.through = Some(position);
        }
        let exclusion = transit
            .projection
            .filter(|&proof| excludes(requested, proof));
        let deadline = expires.min(transit.work.expires_at());
        if fresh
            && let Some(proof) = exclusion
            && deadline > now
            && transit.packet.remaining_hops() > 0
            && self.pending.len() < usize::from(OmissionRun::MAX)
        {
            self.pending.push(PendingExclusion {
                work: transit.work,
                packet: transit.packet.clone().into_owned(),
                proof,
            });
            return;
        }
        if continues
            && let Some(run) = u8::try_from(self.pending.len())
                .ok()
                .and_then(OmissionRun::new)
            && deadline > now
            && transit.packet.remaining_hops() != 0
            && tx.accommodates_omission(position, &transit.packet, run)
        {
            let result = transit.work.enqueue(now, admissions, || {
                tx.try_send_projected(
                    now,
                    deadline,
                    position,
                    transit.packet.clone().into_owned(),
                    Some(run),
                )
            });
            if result == super::LiveOffer::Enqueued {
                report.omitted = report.omitted.saturating_add(self.pending.len());
                self.pending.clear();
                report.record(result);
                return;
            }
        }
        if fresh {
            self.flush(now, tx, admissions, report);
        }
        if let Some(result) = offer_live_network(admissions, tx, expires, transit, now) {
            report.record(result);
        }
    }

    fn flush(
        &mut self,
        now: Instant,
        tx: &WanTx,
        admissions: &mut LiveAdmissions,
        report: &mut Offers,
    ) {
        for PendingExclusion { work, packet, .. } in self.pending.drain(..) {
            let deadline = tx.expires().min(work.expires_at());
            if now < deadline {
                report.record(work.enqueue(now, admissions, || {
                    tx.try_send(now, deadline, work.sequence(), packet)
                }));
            }
        }
    }
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
    Refresh(MediaRepairLookup),
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
            Self::Media(lookup) | Self::Refresh(lookup) => lookup.parent.shard(),
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
    projection: Option<RelayProjection>,
}

struct RelayProjection {
    input: LiveNetworkChild,
    tx: WanTx,
    demand: super::target_demand::Demand,
    buffer: SendProjection,
}

impl RelayProjection {
    fn matches(&self, path: &CompiledLiveRelay) -> bool {
        self.input == path.input && self.tx == path.tx
    }
}

impl LiveRelays {
    pub(crate) fn install(&mut self, now: Instant, relay: CompiledLiveRelay) -> bool {
        if relay.tx.expires() <= now || !relay.tx.is_live() {
            return false;
        }
        let existing = self
            .branches
            .iter()
            .position(|branch| branch.next == relay.next);
        for branch in &mut self.branches {
            branch.paths.retain(|path| path.tx.expires() > now);
            if branch
                .projection
                .as_ref()
                .is_some_and(|projection| now >= projection.tx.expires())
            {
                branch.projection = None;
            }
        }
        if let Some(index) = existing
            && let Some(branch) = self.branches.get_mut(index)
            && let Some(path) = branch
                .paths
                .iter_mut()
                .find(|path| path.input == relay.input)
        {
            if path.tx != relay.tx
                && branch
                    .projection
                    .as_ref()
                    .is_some_and(|projection| projection.matches(path))
            {
                branch.projection = None;
            }
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
                projection: None,
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
            if branch
                .projection
                .as_ref()
                .is_some_and(|projection| projection.matches(relay))
            {
                branch.projection = None;
            }
        }
    }

    pub(crate) fn retire_input(&mut self, address: NodeRouteAddress) {
        for branch in &mut self.branches {
            branch
                .paths
                .retain(|path| path.input.destination != address);
            if branch
                .projection
                .as_ref()
                .is_some_and(|projection| projection.input.destination == address)
            {
                branch.projection = None;
            }
        }
    }

    fn exact_path(branches: &[Branch], route: &CompiledLiveRelay, now: Instant) -> bool {
        route.tx.is_live()
            && branches.iter().any(|branch| {
                branch.next == route.next
                    && branch.paths.iter().any(|path| {
                        path.input == route.input && path.tx == route.tx && now < path.tx.expires()
                    })
            })
    }

    pub(crate) fn apply_demand(
        &mut self,
        now: Instant,
        route: &CompiledLiveRelay,
        update: super::target_demand::VersionedUpdate,
    ) -> bool {
        if !self.has_live_path(route, now) {
            return false;
        }
        self.branches
            .iter_mut()
            .find(|branch| branch.next == route.next)
            .is_some_and(|branch| {
                if branch.paths.len() != 1 {
                    return false;
                }
                if branch
                    .projection
                    .as_ref()
                    .is_some_and(|projection| !projection.matches(route))
                {
                    branch.projection = None;
                }
                let projection = branch.projection.get_or_insert_with(|| RelayProjection {
                    input: route.input,
                    tx: route.tx.clone(),
                    demand: super::target_demand::Demand::default(),
                    buffer: SendProjection::default(),
                });
                projection
                    .demand
                    .apply(now, route.tx.expires(), update.generation, update.update)
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
            if let Some(projection) = branch
                .projection
                .as_mut()
                .filter(|projection| projection.matches(path))
            {
                let requested = (branch.paths.len() == 1)
                    .then(|| projection.demand.live(now))
                    .flatten();
                #[cfg(feature = "sim")]
                let omitted = report.omitted;
                projection.buffer.offer(
                    now,
                    &path.tx,
                    &mut branch.admissions,
                    requested,
                    &frozen,
                    report,
                );
                #[cfg(feature = "sim")]
                if report.omitted != omitted {
                    crate::sim_metrics::record_routing_counter("fabric_wan_relay_projected_runs");
                }
            } else if let Some(outcome) = offer_live_network(
                &mut branch.admissions,
                &path.tx,
                path.tx.expires(),
                &frozen,
                now,
            ) {
                report.record(outcome);
            }
        }
    }
}
