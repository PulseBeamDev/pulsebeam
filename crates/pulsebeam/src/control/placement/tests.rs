#![allow(clippy::unwrap_used)]

use super::*;
use crate::{
    control::controller::ConnectionProfile,
    entity::{ConnectionId, RoomExternalId},
};
use pulsebeam_runtime::rt::WorkerSnapshot;

fn room_id(name: &str) -> RoomId {
    RoomId::from_external(&RoomExternalId::new(name).unwrap())
}

fn commit(
    core: &mut ControllerCore,
    room: RoomId,
    participant: ParticipantId,
    slot: usize,
    shard: ShardId,
) {
    let transport = core.reserve_transport(shard, Instant::now());
    core.registry
        .commit_candidate(
            participant,
            room,
            shard,
            slot,
            transport,
            ConnectionId::new(),
            ConnectionProfile::Native,
            None,
        )
        .unwrap();
}

fn admit(
    core: &ControllerCore,
    room: &RoomId,
    participant: &ParticipantId,
    shards: usize,
    snapshot: Option<&ExecutionSnapshot>,
) -> Option<(usize, ShardId)> {
    let now = snapshot.map_or_else(Instant::now, |snapshot| snapshot.at);
    Placement::default().admit(core, room, participant, shards, snapshot, now)
}

// Rates are thousandths of one worker. Counters use microseconds, timestamps milliseconds.
fn cohort(base: Instant, millis: u64, layout: &[&[(usize, u64)]]) -> ExecutionSnapshot {
    let at = base + Duration::from_millis(millis);
    let epoch = millis + 1;
    ExecutionSnapshot {
        epoch,
        at,
        workers: layout
            .iter()
            .enumerate()
            .map(|(worker, services)| WorkerSnapshot {
                worker,
                epoch,
                scheduling_wait: Duration::ZERO,
                services: services
                    .iter()
                    .map(|&(id, rate)| ServiceSnapshot {
                        id,
                        generation: 0,
                        at,
                        busy: Duration::from_micros(millis * rate),
                        scheduling_wait: Duration::ZERO,
                    })
                    .collect(),
            })
            .collect(),
    }
}

#[test]
fn admission_uses_the_registry_preserves_bindings_and_reuses_empty_partitions() {
    let mut core = ControllerCore::with_shards(4, 2, RoomPartitionPlacement::Balanced);
    let room = room_id("placement");
    let mut participants = Vec::new();
    for index in 0..8 {
        let participant = ParticipantId::new();
        let (slot, shard) = admit(&core, &room, &participant, 4, None).unwrap();
        assert_eq!((slot, shard.index()), (index / 2, index / 2));
        commit(&mut core, room, participant, slot, shard);
        participants.push(participant);
    }
    assert_eq!(
        admit(&core, &room, &participants[0], 4, None),
        Some((0, ShardId::new(0)))
    );
    for participant in &participants[..2] {
        core.registry.remove_participant(participant);
    }
    for _ in 0..4 {
        core.registry.add_participant(
            ParticipantId::new(),
            room_id("other"),
            ShardId::new(0),
            None,
        );
    }
    assert_eq!(
        admit(&core, &room, &ParticipantId::new(), 4, None),
        Some((0, ShardId::new(1)))
    );
}

#[test]
fn uncommitted_admission_leaves_no_reservation_and_explicit_round_robin_is_preserved() {
    let mut core = ControllerCore::with_shards(3, 1, RoomPartitionPlacement::RoundRobin);
    let room = room_id("explicit");
    let first = admit(&core, &room, &ParticipantId::new(), 3, None);
    assert_eq!(first, admit(&core, &room, &ParticipantId::new(), 3, None));
    assert_eq!(core.registry.participants().count(), 0);
    for index in 0..7 {
        let participant = ParticipantId::new();
        let (slot, shard) = admit(&core, &room, &participant, 3, None).unwrap();
        assert_eq!((slot, shard.index()), (index, index % 3));
        commit(&mut core, room, participant, slot, shard);
    }
    assert!(admit(&core, &room, &ParticipantId::new(), 0, None).is_none());
    assert_eq!(initial_worker(ShardId::new(5), 2), Some(1));
    assert_eq!(initial_worker(ShardId::new(5), 0), None);
}

#[test]
fn fallback_reads_actual_residency_without_keeping_a_second_map() {
    let mut core = ControllerCore::with_shards(4, 1, RoomPartitionPlacement::Balanced);
    let room = room_id("cores");
    core.registry
        .add_participant(ParticipantId::new(), room, ShardId::new(0), None);
    let now = Instant::now();
    let snapshot = cohort(now, 0, &[&[(0, 0), (1, 0)], &[(2, 0), (3, 0)]]);
    assert_eq!(
        admit(&core, &room, &ParticipantId::new(), 4, Some(&snapshot)),
        Some((1, ShardId::new(2)))
    );
    let moved = cohort(now, 1, &[&[(0, 0), (2, 0)], &[(1, 0), (3, 0)]]);
    assert_eq!(
        admit(&core, &room, &ParticipantId::new(), 4, Some(&moved)),
        Some((1, ShardId::new(1)))
    );
}

#[test]
fn measured_admission_prefers_a_quieter_worker_even_with_more_participants() {
    let mut core = ControllerCore::with_shards(4, 1, RoomPartitionPlacement::Balanced);
    let existing = room_id("existing");
    core.registry
        .add_participant(ParticipantId::new(), existing, ShardId::new(0), None);
    for shard in [2, 3] {
        for _ in 0..10 {
            core.registry.add_participant(
                ParticipantId::new(),
                existing,
                ShardId::new(shard),
                None,
            );
        }
    }
    let base = Instant::now();
    let layout: &[&[(usize, u64)]] = &[&[(0, 500), (1, 300)], &[(2, 50), (3, 50)]];
    let mut policy = Placement::default();
    for millis in (0..=10_000).step_by(500) {
        let snapshot = cohort(base, millis, layout);
        assert!(policy.rebalance(snapshot.at, &snapshot, 4).is_none());
    }
    let snapshot = cohort(base, 10_000, layout);
    let room = room_id("incoming");
    assert_eq!(
        admit(&core, &room, &ParticipantId::new(), 4, Some(&snapshot)),
        Some((0, ShardId::new(1)))
    );
    assert_eq!(
        policy.admit(
            &core,
            &room,
            &ParticipantId::new(),
            4,
            Some(&snapshot),
            snapshot.at
        ),
        Some((0, ShardId::new(2)))
    );
    let stale_at = snapshot.at + MAX_OBSERVATION_AGE + Duration::from_millis(1);
    assert_eq!(
        policy.admit(
            &core,
            &room,
            &ParticipantId::new(),
            4,
            Some(&snapshot),
            stale_at
        ),
        Some((0, ShardId::new(1)))
    );
}

#[test]
fn elapsed_time_not_sample_count_controls_demand_and_candidate_persistence() {
    let base = Instant::now();
    for cadence in [250, 1000] {
        let mut policy = Placement::default();
        for millis in (0..=20_000).step_by(cadence) {
            let snapshot = cohort(base, millis, &[&[(0, 550), (1, 350)], &[(2, 50)]]);
            let movement = policy.rebalance(snapshot.at, &snapshot, 3);
            if millis < 20_000 {
                assert!(movement.is_none());
            } else {
                assert_eq!(
                    movement,
                    Some(ShardMove {
                        shard: 1,
                        source: 0,
                        destination: 1
                    })
                );
            }
            assert!(policy.rebalance(snapshot.at, &snapshot, 3).is_none());
        }
    }
}

#[test]
fn lone_hot_shards_weak_gains_and_busy_destinations_do_not_qualify() {
    let base = Instant::now();
    let cases: &[&[&[(usize, u64)]]] = &[
        &[&[(0, 900)], &[(1, 50)]],
        &[&[(0, 660), (1, 90)], &[(2, 300)]],
        &[&[(0, 550), (1, 350)], &[(2, 500)]],
        &[&[(0, 350), (1, 300)], &[(2, 50)]],
    ];
    for layout in cases {
        let shards = layout.iter().map(|worker| worker.len()).sum();
        let mut policy = Placement::default();
        for millis in (0..=30_000).step_by(500) {
            let snapshot = cohort(base, millis, layout);
            assert!(policy.rebalance(snapshot.at, &snapshot, shards).is_none());
        }
    }
}

#[test]
fn an_isolated_media_burst_does_not_authorize_a_move() {
    let base = Instant::now();
    let mut policy = Placement::default();
    for millis in (0..=30_000).step_by(500) {
        let mut snapshot = cohort(base, millis, &[&[(0, 100), (1, 100)], &[(2, 50)]]);
        for service in &mut snapshot.workers[0].services {
            let extra = if service.id == 0 { 450 } else { 250 };
            service.busy += Duration::from_micros(millis.min(2000) * extra);
        }
        assert!(policy.rebalance(snapshot.at, &snapshot, 3).is_none());
    }
}

#[test]
fn partial_stale_and_reset_observations_cannot_reuse_qualified_history() {
    let base = Instant::now();
    let layout: &[&[(usize, u64)]] = &[&[(0, 550), (1, 350)], &[(2, 50)]];
    for bad_input in 0..5 {
        let mut policy = Placement::default();
        for millis in (0..=19_500).step_by(500) {
            let snapshot = cohort(base, millis, layout);
            assert!(policy.rebalance(snapshot.at, &snapshot, 3).is_none());
        }
        let mut snapshot = cohort(base, 20_000, layout);
        let now = match bad_input {
            0 => {
                snapshot.workers.pop();
                snapshot.at
            }
            1 => snapshot.at + MAX_OBSERVATION_AGE + Duration::from_millis(1),
            2 => {
                snapshot.workers[0].services[0].generation = 1;
                snapshot.at
            }
            3 => {
                snapshot.workers[0].services[0].busy = Duration::ZERO;
                snapshot.at
            }
            _ => {
                snapshot.workers[0].services[0].at -= Duration::from_secs(2);
                snapshot.at
            }
        };
        assert!(policy.rebalance(now, &snapshot, 3).is_none());
        assert!(policy.candidate.is_none());
        assert!(policy.history.is_empty());
    }
}

#[test]
fn attempted_moves_exclude_warmup_and_balanced_settled_work_stops_movement() {
    let base = Instant::now();
    let mut policy = Placement::default();
    for millis in (0..=20_000).step_by(500) {
        let snapshot = cohort(base, millis, &[&[(0, 550), (1, 350)], &[(2, 50)]]);
        let movement = policy.rebalance(snapshot.at, &snapshot, 3);
        if millis == 20_000 {
            assert!(movement.is_some());
        }
    }
    policy.attempted(base + Duration::from_secs(20));
    for millis in (20_500..50_000).step_by(500) {
        let snapshot = cohort(base, millis, &[&[(0, 550)], &[(1, 900), (2, 50)]]);
        assert!(policy.rebalance(snapshot.at, &snapshot, 3).is_none());
        assert!(policy.history.is_empty());
    }
    for millis in (50_000..=90_000).step_by(500) {
        let snapshot = cohort(base, millis, &[&[(0, 550)], &[(1, 350), (2, 50)]]);
        assert!(policy.rebalance(snapshot.at, &snapshot, 3).is_none());
    }
    assert!(policy.candidate.is_none());
}
