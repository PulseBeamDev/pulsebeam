use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroU64,
};

use pulsebeam_proto::data_record::MAX_FRAGMENTS;
use pulsebeam_runtime::time::{Duration, Instant};

use super::{FabricBranchHandle, MAX_BRANCHES};
use crate::{
    keys::ParticipantHandle,
    route::fabric::{ShardBinding, ShardDataExpectation, ShardDataReceiver},
    shard_update::CompiledRoute,
};

const UNIT_LIMIT: usize = 64;
const WINDOW: u64 = 512;
pub(super) const LIFETIME: Duration = Duration::from_secs(2);
pub(super) const RETRY: Duration = Duration::from_millis(100);

type Child = (FabricBranchHandle, ParticipantHandle);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DataExpectation {
    pub(crate) unit_seq: u64,
    pub(crate) fragment_count: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExpectationAdmission {
    First,
    Replay,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExpectationError {
    Binding,
    InvalidCount,
    Retired,
    Capacity,
    Conflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PropagationOutcome {
    Confirmed,
    Failed,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExpectationWrite {
    Queued,
    Backpressured,
    Revoked,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PropagationStep {
    Attempted,
    Settled {
        unit: DataExpectation,
        branch: FabricBranchHandle,
        destination: ParticipantHandle,
        outcome: PropagationOutcome,
    },
}

#[derive(Clone, Copy)]
enum Propagation {
    Pending { retry_at: Instant },
    Settled(PropagationOutcome),
    Reported(PropagationOutcome),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReceiptAcceptance {
    First,
    Replay,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeliveryOutcome {
    Confirmed,
    Failed,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DeliveryReport {
    pub(crate) unit: DataExpectation,
    pub(crate) branch_incarnation: NonZeroU64,
    pub(crate) outcome: DeliveryOutcome,
}

#[derive(Clone, Copy)]
enum Delivery {
    Awaiting,
    Ready(DeliveryOutcome),
    Reported(DeliveryOutcome),
}

struct ChildState {
    binding: Child,
    branch_incarnation: NonZeroU64,
    propagation: Propagation,
    delivery: Delivery,
    sent: bool,
}

pub(crate) struct ShardDataCohort {
    pub(crate) binding: ShardBinding,
    pub(crate) route: CompiledRoute,
    pub(crate) receivers: Box<[ShardDataReceiver]>,
}

struct ShardState {
    cohort: ShardDataCohort,
    deliveries: Box<[Delivery]>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DeliverySlot {
    Local(usize),
    Shard(usize, usize),
}

struct Open {
    expires: Instant,
    fragment_count: u16,
    children: Box<[ChildState]>,
    shards: Box<[ShardState]>,
    pending_shards: usize,
}

impl Open {
    fn has_work(&self) -> bool {
        self.pending_shards != 0
            || self.shards.iter().any(|shard| {
                shard
                    .deliveries
                    .iter()
                    .any(|delivery| !matches!(delivery, Delivery::Reported(_)))
            })
            || self.children.iter().any(|child| {
                !matches!(child.propagation, Propagation::Reported(_))
                    || !matches!(child.delivery, Delivery::Reported(_))
            })
    }
}

#[derive(Default)]
pub(super) struct Expectations {
    high: Option<u64>,
    entries: BTreeMap<u64, Option<Open>>,
    scheduled: BTreeSet<(Instant, u64, usize)>,
    deliveries: BTreeSet<(Instant, u64, DeliverySlot)>,
    notices: BTreeSet<(Instant, u64, usize)>,
}

impl Expectations {
    pub(super) fn accept<I: IntoIterator<Item = (Child, NonZeroU64)>>(
        &mut self,
        now: Instant,
        source_deadline: Option<Instant>,
        expectation: DataExpectation,
        snapshot: impl FnOnce() -> I,
        shards: impl FnOnce() -> Vec<ShardDataCohort>,
    ) -> Result<ExpectationAdmission, ExpectationError> {
        let DataExpectation {
            unit_seq,
            fragment_count,
        } = expectation;
        if !(1..=MAX_FRAGMENTS).contains(&fragment_count) {
            return Err(ExpectationError::InvalidCount);
        }
        if source_deadline.is_some_and(|deadline| now >= deadline) {
            return Err(ExpectationError::Retired);
        }
        let high = self.high.map_or(unit_seq, |high| high.max(unit_seq));
        let floor = high.saturating_sub(WINDOW.saturating_sub(1));
        if unit_seq < floor {
            return Err(ExpectationError::Retired);
        }
        if let Some(entry) = self.entries.get(&unit_seq) {
            let open = entry
                .as_ref()
                .filter(|open| now < open.expires)
                .ok_or(ExpectationError::Retired)?;
            return if open.fragment_count == fragment_count {
                Ok(ExpectationAdmission::Replay)
            } else {
                Err(ExpectationError::Conflict)
            };
        }
        let expires = now
            .checked_add(LIFETIME)
            .ok_or(ExpectationError::Capacity)?;
        let expires = source_deadline.map_or(expires, |deadline| deadline.min(expires));
        if self
            .entries
            .range(..floor)
            .any(|(_, entry)| entry.as_ref().is_some_and(Open::has_work))
        {
            return Err(ExpectationError::Capacity);
        }
        let mut live = 0_usize;
        for (&_seq, entry) in self.entries.range_mut(floor..) {
            if let Some(open) = entry {
                if now >= open.expires && !open.has_work() {
                    #[cfg(feature = "sim")]
                    if !open.shards.is_empty() {
                        crate::sim_metrics::record_expired_shard_data_reclaimed(_seq);
                    }
                    *entry = None;
                } else {
                    live = live.saturating_add(1);
                }
            }
        }
        if live >= UNIT_LIMIT {
            return Err(ExpectationError::Capacity);
        }
        let children: Box<[_]> = snapshot()
            .into_iter()
            .take(MAX_BRANCHES + 1)
            .map(|(binding, branch_incarnation)| ChildState {
                binding,
                branch_incarnation,
                propagation: Propagation::Pending { retry_at: now },
                delivery: Delivery::Awaiting,
                sent: false,
            })
            .collect();
        let shards = shards();
        let leaves = shards.iter().fold(children.len(), |count, shard| {
            count.saturating_add(shard.receivers.len())
        });
        if leaves > MAX_BRANCHES || shards.iter().any(|shard| shard.receivers.is_empty()) {
            return Err(ExpectationError::Capacity);
        }
        while self
            .entries
            .first_key_value()
            .is_some_and(|(&seq, _)| seq < floor)
        {
            let _ = self.entries.pop_first();
        }
        self.high = Some(high);
        for index in 0..children.len() {
            self.scheduled.insert((now, unit_seq, index));
            self.deliveries
                .insert((expires, unit_seq, DeliverySlot::Local(index)));
        }
        let shards: Box<[_]> = shards
            .into_iter()
            .enumerate()
            .map(|(index, cohort)| {
                self.notices.insert((now, unit_seq, index));
                for member in 0..cohort.receivers.len() {
                    self.deliveries
                        .insert((expires, unit_seq, DeliverySlot::Shard(index, member)));
                }
                let deliveries =
                    vec![Delivery::Awaiting; cohort.receivers.len()].into_boxed_slice();
                ShardState { cohort, deliveries }
            })
            .collect();
        self.entries.insert(
            unit_seq,
            Some(Open {
                expires,
                fragment_count,
                children,
                pending_shards: shards.len(),
                shards,
            }),
        );
        Ok(ExpectationAdmission::First)
    }

    pub(super) fn has_shard(
        &self,
        now: Instant,
        unit: DataExpectation,
        binding: ShardBinding,
        route: &CompiledRoute,
    ) -> bool {
        self.entries
            .get(&unit.unit_seq)
            .and_then(Option::as_ref)
            .is_some_and(|open| {
                now < open.expires
                    && open.fragment_count == unit.fragment_count
                    && open.shards.iter().any(|shard| {
                        shard.cohort.binding == binding && shard.cohort.route == *route
                    })
            })
    }

    pub(super) fn shard_repair_window(
        &self,
        now: Instant,
        route: &CompiledRoute,
        repair: super::super::ShardDataRepair,
    ) -> Option<u16> {
        let open = self
            .entries
            .get(&repair.request.fragment.unit_seq)?
            .as_ref()?;
        if now >= open.expires || repair.request.fragment.index >= open.fragment_count {
            return None;
        }
        let shard = open
            .shards
            .iter()
            .find(|shard| shard.cohort.binding == repair.binding && shard.cohort.route == *route)?;
        let index = shard
            .cohort
            .receivers
            .iter()
            .position(|receiver| *receiver == repair.receiver)?;
        matches!(shard.deliveries.get(index), Some(Delivery::Awaiting))
            .then_some(open.fragment_count)
    }

    pub(super) fn notice_deadline(&self) -> Option<Instant> {
        self.notices.first().map(|&(at, _, _)| at)
    }

    pub(super) fn tick_notice(
        &mut self,
        now: Instant,
        send: impl FnOnce(CompiledRoute, ShardDataExpectation),
    ) -> Option<()> {
        let &(at, seq, index) = self.notices.first()?;
        if now < at {
            return None;
        }
        self.send_notice(now, (at, seq, index), send)
    }

    pub(super) fn send_initial_notices(
        &mut self,
        now: Instant,
        unit: DataExpectation,
        mut send: impl FnMut(CompiledRoute, ShardDataExpectation),
    ) {
        let Some(open) = self.entries.get(&unit.unit_seq).and_then(Option::as_ref) else {
            return;
        };
        for index in 0..open.shards.len() {
            self.send_notice(now, (now, unit.unit_seq, index), &mut send);
        }
    }

    fn send_notice(
        &mut self,
        now: Instant,
        key: (Instant, u64, usize),
        send: impl FnOnce(CompiledRoute, ShardDataExpectation),
    ) -> Option<()> {
        if !self.notices.remove(&key) {
            return None;
        }
        let (_, seq, index) = key;
        let open = self.entries.get_mut(&seq)?.as_mut()?;
        let shard = &open.shards.get(index)?.cohort;
        if now >= open.expires || !shard.route.tx.is_live() {
            open.pending_shards = open.pending_shards.saturating_sub(1);
            return Some(());
        }
        let retry_at = now
            .checked_add(RETRY)
            .map_or(open.expires, |retry| retry.min(open.expires));
        self.notices.insert((retry_at, seq, index));
        send(
            shard.route.clone(),
            ShardDataExpectation {
                binding: shard.binding,
                unit: DataExpectation {
                    unit_seq: seq,
                    fragment_count: open.fragment_count,
                },
                expires: open.expires,
                receivers: shard.receivers.clone(),
            },
        );
        Some(())
    }

    pub(super) fn for_child(
        &self,
        now: Instant,
        unit_seq: u64,
        child: Child,
    ) -> Option<(DataExpectation, NonZeroU64)> {
        let open = self
            .entries
            .get(&unit_seq)?
            .as_ref()
            .filter(|open| now < open.expires)?;
        let leaf = open.children.iter().find(|state| state.binding == child)?;
        Some((
            DataExpectation {
                unit_seq,
                fragment_count: open.fragment_count,
            },
            leaf.branch_incarnation,
        ))
    }

    pub(super) fn repair_until(
        &self,
        now: Instant,
        fragment: super::RepairFragment,
        binding: Child,
        incarnation: NonZeroU64,
    ) -> Option<Instant> {
        let open = self.entries.get(&fragment.unit_seq)?.as_ref()?;
        if now >= open.expires || fragment.index >= open.fragment_count {
            return None;
        }
        open.children
            .iter()
            .any(|child| {
                child.binding == binding
                    && child.branch_incarnation == incarnation
                    && child.sent
                    && matches!(child.delivery, Delivery::Awaiting)
                    && matches!(
                        child.propagation,
                        Propagation::Settled(PropagationOutcome::Confirmed)
                            | Propagation::Reported(PropagationOutcome::Confirmed)
                    )
            })
            .then_some(open.expires)
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        self.scheduled.first().map(|&(deadline, _, _)| deadline)
    }

    pub(super) fn tick(
        &mut self,
        now: Instant,
        write: impl FnOnce(Child, DataExpectation, NonZeroU64) -> ExpectationWrite,
    ) -> Option<PropagationStep> {
        let &(deadline, unit_seq, index) = self.scheduled.first()?;
        if now < deadline {
            return None;
        }
        self.scheduled.pop_first();
        let open = self.entries.get_mut(&unit_seq)?.as_mut()?;
        let unit = DataExpectation {
            unit_seq,
            fragment_count: open.fragment_count,
        };
        let child = open.children.get_mut(index)?;
        let outcome = match child.propagation {
            Propagation::Settled(outcome) => outcome,
            Propagation::Pending { .. } if now >= open.expires => PropagationOutcome::Unknown,
            Propagation::Pending { .. } => {
                match write(child.binding, unit, child.branch_incarnation) {
                    ExpectationWrite::Revoked => PropagationOutcome::Failed,
                    result => {
                        let retry_at = now
                            .checked_add(RETRY)
                            .map_or(open.expires, |retry| retry.min(open.expires));
                        child.sent |= result == ExpectationWrite::Queued;
                        child.propagation = Propagation::Pending { retry_at };
                        self.scheduled.insert((retry_at, unit_seq, index));
                        return Some(PropagationStep::Attempted);
                    }
                }
            }
            Propagation::Reported(_) => return None,
        };
        child.propagation = Propagation::Reported(outcome);
        if outcome == PropagationOutcome::Failed && matches!(child.delivery, Delivery::Awaiting) {
            let slot = DeliverySlot::Local(index);
            self.deliveries.remove(&(open.expires, unit_seq, slot));
            child.delivery = Delivery::Ready(DeliveryOutcome::Failed);
            self.deliveries.insert((now, unit_seq, slot));
        }
        let (branch, destination) = child.binding;
        Some(PropagationStep::Settled {
            unit,
            branch,
            destination,
            outcome,
        })
    }

    pub(super) fn settle(
        &mut self,
        now: Instant,
        unit: DataExpectation,
        binding: Child,
        outcome: PropagationOutcome,
    ) -> bool {
        let Some(open) = self
            .entries
            .get_mut(&unit.unit_seq)
            .and_then(Option::as_mut)
        else {
            return false;
        };
        if open.fragment_count != unit.fragment_count || now >= open.expires {
            return false;
        }
        let Some((index, child)) = open
            .children
            .iter_mut()
            .enumerate()
            .find(|(_, child)| child.binding == binding)
        else {
            return false;
        };
        let Propagation::Pending { retry_at } = child.propagation else {
            return false;
        };
        if !child.sent || !self.scheduled.remove(&(retry_at, unit.unit_seq, index)) {
            return false;
        }
        child.propagation = Propagation::Settled(outcome);
        self.scheduled.insert((now, unit.unit_seq, index));
        true
    }

    pub(super) fn record_receipt(
        &mut self,
        now: Instant,
        unit: DataExpectation,
        binding: Child,
        branch_incarnation: NonZeroU64,
    ) -> Option<ReceiptAcceptance> {
        let open = self.entries.get_mut(&unit.unit_seq)?.as_mut()?;
        if now >= open.expires || open.fragment_count != unit.fragment_count {
            return None;
        }
        let (index, child) = open.children.iter_mut().enumerate().find(|(_, child)| {
            child.binding == binding && child.branch_incarnation == branch_incarnation && child.sent
        })?;
        Self::confirm_delivery(
            &mut self.deliveries,
            now,
            open.expires,
            unit.unit_seq,
            DeliverySlot::Local(index),
            &mut child.delivery,
        )
    }

    pub(super) fn record_shard_receipt(
        &mut self,
        now: Instant,
        binding: ShardBinding,
        route: &CompiledRoute,
        unit: DataExpectation,
        incarnation: NonZeroU64,
    ) -> Option<ReceiptAcceptance> {
        let open = self.entries.get_mut(&unit.unit_seq)?.as_mut()?;
        if now >= open.expires || open.fragment_count != unit.fragment_count {
            return None;
        }
        let (index, shard) =
            open.shards.iter_mut().enumerate().find(|(_, shard)| {
                shard.cohort.binding == binding && shard.cohort.route == *route
            })?;
        let member = shard
            .cohort
            .receivers
            .iter()
            .position(|receiver| receiver.branch == incarnation)?;
        Self::confirm_delivery(
            &mut self.deliveries,
            now,
            open.expires,
            unit.unit_seq,
            DeliverySlot::Shard(index, member),
            shard.deliveries.get_mut(member)?,
        )
    }

    fn confirm_delivery(
        deliveries: &mut BTreeSet<(Instant, u64, DeliverySlot)>,
        now: Instant,
        expires: Instant,
        seq: u64,
        slot: DeliverySlot,
        delivery: &mut Delivery,
    ) -> Option<ReceiptAcceptance> {
        match *delivery {
            Delivery::Awaiting => {
                deliveries.remove(&(expires, seq, slot));
                *delivery = Delivery::Ready(DeliveryOutcome::Confirmed);
                deliveries.insert((now, seq, slot));
                Some(ReceiptAcceptance::First)
            }
            Delivery::Ready(DeliveryOutcome::Confirmed)
            | Delivery::Reported(DeliveryOutcome::Confirmed) => Some(ReceiptAcceptance::Replay),
            _ => None,
        }
    }

    pub(super) fn delivery_deadline(&self) -> Option<Instant> {
        self.deliveries.first().map(|&(deadline, _, _)| deadline)
    }

    pub(super) fn tick_delivery(
        &mut self,
        now: Instant,
        write: impl FnOnce(DeliveryReport) -> ExpectationWrite,
    ) -> Option<()> {
        let &(scheduled, unit_seq, slot) = self.deliveries.first()?;
        if now < scheduled {
            return None;
        }
        self.deliveries.pop_first();
        let open = self.entries.get_mut(&unit_seq)?.as_mut()?;
        let (incarnation, delivery) = match slot {
            DeliverySlot::Local(index) => {
                let child = open.children.get_mut(index)?;
                (child.branch_incarnation, &mut child.delivery)
            }
            DeliverySlot::Shard(index, member) => {
                let shard = open.shards.get_mut(index)?;
                (
                    shard.cohort.receivers.get(member)?.branch,
                    shard.deliveries.get_mut(member)?,
                )
            }
        };
        let outcome = match *delivery {
            Delivery::Awaiting => DeliveryOutcome::Unknown,
            Delivery::Ready(outcome) => outcome,
            Delivery::Reported(_) => return None,
        };
        let end = open.expires.checked_add(RETRY).unwrap_or(open.expires);
        let report = DeliveryReport {
            unit: DataExpectation {
                unit_seq,
                fragment_count: open.fragment_count,
            },
            branch_incarnation: incarnation,
            outcome,
        };
        if now <= end && write(report) == ExpectationWrite::Backpressured && now < end {
            let retry_at = now.checked_add(RETRY).map_or(end, |retry| retry.min(end));
            *delivery = Delivery::Ready(outcome);
            self.deliveries.insert((retry_at, unit_seq, slot));
        } else {
            *delivery = Delivery::Reported(outcome);
        }
        Some(())
    }
}
