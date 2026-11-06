use pulsebeam_runtime::{
    rt::{ExecutionSnapshot, ServiceSnapshot},
    time::Instant,
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::Duration,
};

use super::core::{ControllerCore, RoomPartitionPlacement};
use crate::{
    entity::{ParticipantId, RoomId},
    id::ShardId,
};

const MAX_OBSERVATION_AGE: Duration = Duration::from_secs(1);
const WINDOW: Duration = Duration::from_secs(2);
const HISTORY_WINDOWS: usize = 5;
const PERSISTENCE: Duration = Duration::from_secs(10);
const SETTLING_HOLD: Duration = Duration::from_secs(30);
// Fractions of one worker. These margins are heuristics, not measured migration-cost bounds.
const MIN_SOURCE: f64 = 0.70;
const MAX_DESTINATION: f64 = 0.70;
const MIN_GAIN: f64 = 0.10;
const RELATIVE_GAIN: f64 = 0.20;
const MOVE_RESERVE: f64 = 0.05;
const ADMISSION_BAND: f64 = 0.05;

pub(crate) fn initial_worker(shard: ShardId, workers: usize) -> Option<usize> {
    shard.index().checked_rem(workers)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct ShardMove {
    pub shard: usize,
    pub source: usize,
    pub destination: usize,
}

#[derive(Clone, Copy)]
struct Bounds {
    lower: f64,
    upper: f64,
}

impl Bounds {
    fn add(self, other: Self) -> Self {
        Self {
            lower: self.lower + other.lower,
            upper: self.upper + other.upper,
        }
    }
}

struct Demand {
    generation: u64,
    busy: Duration,
    at: Instant,
    partial_busy: Duration,
    partial_elapsed: Duration,
    windows: VecDeque<f64>,
    latest: f64,
}

impl Demand {
    fn new(sample: &ServiceSnapshot) -> Self {
        Self {
            generation: sample.generation,
            busy: sample.busy,
            at: sample.at,
            partial_busy: Duration::ZERO,
            partial_elapsed: Duration::ZERO,
            windows: VecDeque::with_capacity(HISTORY_WINDOWS),
            latest: 0.0,
        }
    }

    fn record(&mut self, sample: &ServiceSnapshot) -> bool {
        let Some(elapsed) = sample
            .at
            .checked_duration_since(self.at)
            .filter(|elapsed| !elapsed.is_zero())
        else {
            return false;
        };
        let Some(busy) = sample.busy.checked_sub(self.busy) else {
            return false;
        };
        if elapsed > MAX_OBSERVATION_AGE || busy > elapsed {
            return false;
        }
        self.latest = busy.as_secs_f64() / elapsed.as_secs_f64();
        self.partial_busy = self.partial_busy.saturating_add(busy);
        self.partial_elapsed = self.partial_elapsed.saturating_add(elapsed);
        self.busy = sample.busy;
        self.at = sample.at;
        if self.partial_elapsed >= WINDOW {
            if self.windows.len() == HISTORY_WINDOWS {
                self.windows.pop_front();
            }
            self.windows
                .push_back(self.partial_busy.as_secs_f64() / self.partial_elapsed.as_secs_f64());
            self.partial_busy = Duration::ZERO;
            self.partial_elapsed = Duration::ZERO;
        }
        true
    }

    fn bounds(&self) -> Option<Bounds> {
        if self.windows.len() != HISTORY_WINDOWS {
            return None;
        }
        Some(Bounds {
            lower: self.windows.iter().copied().fold(f64::INFINITY, f64::min),
            upper: self.windows.iter().copied().fold(0.0, f64::max),
        })
    }
}

/// Measurement history and policy timing only. Ownership stays in the registry and runtime.
#[derive(Default)]
pub(super) struct Placement {
    history: HashMap<usize, Demand>,
    last_epoch: u64,
    candidate: Option<(ShardMove, Instant)>,
    hold_until: Option<Instant>,
}

impl Placement {
    /// Admissions serialize through materialization; there is no separate reservation ledger.
    pub fn admit(
        &self,
        core: &ControllerCore,
        room: &RoomId,
        participant: &ParticipantId,
        shards: usize,
        execution: Option<&ExecutionSnapshot>,
        now: Instant,
    ) -> Option<(usize, ShardId)> {
        let (slot, mode) = core.next_room_partition(room, participant);
        if let Some(current) = core.registry.get_participant(participant) {
            return Some((slot, current.shard_id));
        }
        if let Some(owner) = core
            .registry
            .get_room_shard(room)
            .and_then(|room| room.partition_owner(slot))
        {
            return Some((slot, owner));
        }
        if mode == RoomPartitionPlacement::RoundRobin {
            return Some((slot, ShardId::new(slot.checked_rem(shards)?)));
        }
        let mut counts = vec![0usize; shards];
        for participant in core.registry.participants() {
            if let Some(count) = counts.get_mut(participant.shard_id.index()) {
                *count = count.saturating_add(1);
            }
        }
        let execution = execution.filter(|snapshot| usable(now, snapshot, shards));
        let measured = execution.and_then(|snapshot| self.estimates(now, snapshot, shards, true));
        let worker_counts: Vec<_> = execution
            .into_iter()
            .flat_map(|snapshot| &snapshot.workers)
            .map(|worker| {
                worker
                    .services
                    .iter()
                    .filter_map(|service| counts.get(service.id))
                    .sum::<usize>()
            })
            .collect();
        let score = |shard: usize, count: usize| {
            let worker = execution.and_then(|snapshot| {
                snapshot
                    .workers
                    .iter()
                    .position(|worker| worker.services.iter().any(|service| service.id == shard))
            });
            let worker_count = worker
                .and_then(|worker| worker_counts.get(worker))
                .copied()
                .unwrap_or(count);
            let worker_load = worker
                .and_then(|worker| measured.as_ref()?.get(worker))
                .map_or(0.0, |load| load.upper);
            let shard_load = measured
                .as_ref()
                .and_then(|_| self.history.get(&shard))
                .and_then(Demand::bounds)
                .map_or(0.0, |load| load.upper);
            (
                (worker_load / ADMISSION_BAND).ceil(),
                worker_count,
                (shard_load / ADMISSION_BAND).ceil(),
                count,
                shard,
            )
        };
        let (owner, _) = counts.iter().enumerate().min_by(|&(a, &ac), &(b, &bc)| {
            let a = score(a, ac);
            let b = score(b, bc);
            a.0.total_cmp(&b.0)
                .then(a.1.cmp(&b.1))
                .then(a.2.total_cmp(&b.2))
                .then(a.3.cmp(&b.3))
                .then(a.4.cmp(&b.4))
        })?;
        Some((slot, ShardId::new(owner)))
    }

    pub fn rebalance(
        &mut self,
        now: Instant,
        snapshot: &ExecutionSnapshot,
        shards: usize,
    ) -> Option<ShardMove> {
        if snapshot.epoch == self.last_epoch {
            return None;
        }
        if snapshot.epoch < self.last_epoch || !usable(now, snapshot, shards) {
            self.history.clear();
            self.candidate = None;
            return None;
        }
        self.last_epoch = snapshot.epoch;
        if let Some(until) = self.hold_until {
            if now < until
                || snapshot
                    .workers
                    .iter()
                    .flat_map(|worker| &worker.services)
                    .any(|sample| sample.at < until)
            {
                self.history.clear();
                self.candidate = None;
                return None;
            }
            self.hold_until = None;
        }
        // Envelope counters restart even when registration fails and the service rolls back.
        if snapshot
            .workers
            .iter()
            .flat_map(|worker| &worker.services)
            .any(|sample| {
                self.history.get(&sample.id).is_some_and(|previous| {
                    previous.generation != sample.generation || sample.busy < previous.busy
                })
            })
        {
            self.attempted(now);
            return None;
        }
        for sample in snapshot.workers.iter().flat_map(|worker| &worker.services) {
            if let Some(demand) = self.history.get_mut(&sample.id) {
                if !demand.record(sample) {
                    self.history.clear();
                    self.candidate = None;
                    return None;
                }
            } else {
                self.history.insert(sample.id, Demand::new(sample));
            }
        }
        let Some(loads) = self.estimates(now, snapshot, shards, false) else {
            self.candidate = None;
            return None;
        };
        let mut best: Option<(ShardMove, f64)> = None;
        for source in &snapshot.workers {
            let Some(source_load) = loads.get(source.worker) else {
                continue;
            };
            if source_load.lower < MIN_SOURCE {
                continue;
            }
            for sample in &source.services {
                let Some(shard_load) = self.history.get(&sample.id).and_then(Demand::bounds) else {
                    continue;
                };
                for destination in &snapshot.workers {
                    if source.worker == destination.worker {
                        continue;
                    }
                    let Some(destination_load) = loads.get(destination.worker) else {
                        continue;
                    };
                    let destination_after =
                        destination_load.upper + shard_load.upper + MOVE_RESERVE;
                    let source_after = (source_load.upper - shard_load.upper).max(0.0);
                    let gain = source_load.lower - source_after.max(destination_after);
                    if destination_after > MAX_DESTINATION
                        || gain < MIN_GAIN.max(RELATIVE_GAIN * source_load.lower)
                    {
                        continue;
                    }
                    let movement = ShardMove {
                        shard: sample.id,
                        source: source.worker,
                        destination: destination.worker,
                    };
                    if best.is_none_or(|(previous, previous_gain)| {
                        gain.total_cmp(&previous_gain).is_gt()
                            || (gain.total_cmp(&previous_gain).is_eq() && movement < previous)
                    }) {
                        best = Some((movement, gain));
                    }
                }
            }
        }
        let Some((movement, _)) = best else {
            self.candidate = None;
            return None;
        };
        let since = match self.candidate {
            Some((previous, since)) if previous == movement => since,
            _ => now,
        };
        self.candidate = Some((movement, since));
        (now.saturating_duration_since(since) >= PERSISTENCE).then_some(movement)
    }

    pub fn attempted(&mut self, now: Instant) {
        self.hold_until = Some(now.checked_add(SETTLING_HOLD).unwrap_or(now));
        self.history.clear();
        self.candidate = None;
    }

    fn estimates(
        &self,
        now: Instant,
        snapshot: &ExecutionSnapshot,
        shards: usize,
        admission: bool,
    ) -> Option<Vec<Bounds>> {
        if self.hold_until.is_some_and(|until| now < until) || !usable(now, snapshot, shards) {
            return None;
        }
        snapshot
            .workers
            .iter()
            .map(|worker| {
                worker.services.iter().try_fold(
                    Bounds {
                        lower: 0.0,
                        upper: 0.0,
                    },
                    |total, sample| {
                        let demand = self.history.get(&sample.id)?;
                        if sample.generation != demand.generation
                            || sample.at < demand.at
                            || now.saturating_duration_since(demand.at) > MAX_OBSERVATION_AGE
                        {
                            return None;
                        }
                        let mut bounds = demand.bounds()?;
                        if admission {
                            bounds.upper = bounds.upper.max(demand.latest);
                            let busy = sample.busy.checked_sub(demand.busy)?;
                            let elapsed = sample.at.saturating_duration_since(demand.at);
                            if !elapsed.is_zero() {
                                if busy > elapsed {
                                    return None;
                                }
                                bounds.upper =
                                    bounds.upper.max(busy.as_secs_f64() / elapsed.as_secs_f64());
                            }
                        }
                        Some(total.add(bounds))
                    },
                )
            })
            .collect()
    }
}

fn usable(now: Instant, snapshot: &ExecutionSnapshot, shards: usize) -> bool {
    if snapshot.at > now
        || now.saturating_duration_since(snapshot.at) > MAX_OBSERVATION_AGE
        || snapshot.workers.is_empty()
    {
        return false;
    }
    let mut seen = HashSet::with_capacity(shards);
    snapshot.workers.iter().enumerate().all(|(index, worker)| {
        worker.worker == index
            && worker.epoch == snapshot.epoch
            && worker.services.iter().all(|sample| {
                sample.id < shards
                    && sample.at <= snapshot.at
                    && now.saturating_duration_since(sample.at) <= MAX_OBSERVATION_AGE
                    && seen.insert(sample.id)
            })
    }) && seen.len() == shards
}

#[cfg(test)]
mod tests;
