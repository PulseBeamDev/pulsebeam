use std::cell::RefCell;
use std::hint::black_box;
use std::time::{Duration, Instant as CpuInstant, SystemTime};

use slotmap::SlotMap;
use tokio::time::Instant;

use crate::clock::WallAnchor;
use crate::id::ShardId;
use crate::keys::TrackKey;
use crate::route::{Envelope, RouteHandle, RouteId};
use crate::rtp::RtpPacket;
use crate::shard::core::ShardCore;
use crate::shard::dirty::DirtyTracker;
use crate::shard::participants::ParticipantRegistry;
use crate::shard::router::{ForwardingContext, Origin, ShardRuntime, ShardTransport};
use crate::shard::worker::{MediaPayload, ShardFrame};
use crate::shard_update::{ShardUpdateOp, TrackPlan, TrackRuntime};

#[derive(Debug, Clone, Copy)]
pub struct Percentiles {
    pub p50_ns: u128,
    pub p99_ns: u128,
    pub p999_ns: u128,
    pub p9999_ns: u128,
    pub max_ns: u128,
}

#[derive(Debug, Clone, Copy)]
pub struct Report {
    pub steady: Percentiles,
    pub churn: Percentiles,
    pub samples: usize,
    pub churn_every: usize,
}

struct NullTransport;

impl ShardTransport for NullTransport {
    fn send_media(&self, _dst: ShardId, _env: Envelope, _payload: MediaPayload) {}

    fn send_frame(&self, _dst: ShardId, _frame: ShardFrame) {}
}

struct DestinationTransport {
    destination: RefCell<ShardCore>,
    null: NullTransport,
}

impl ShardTransport for DestinationTransport {
    fn send_media(&self, dst: ShardId, env: Envelope, payload: MediaPayload) {
        debug_assert_eq!(dst, ShardId::new(1));
        self.destination.borrow_mut().benchmark_media_frame(
            ShardFrame::Media { env, payload },
            Instant::now(),
            &self.null,
        );
    }

    fn send_frame(&self, _dst: ShardId, _frame: ShardFrame) {}
}

struct Fixture {
    source: ShardRuntime,
    source_key: TrackKey,
    destination_key: TrackKey,
    source_plan: TrackPlan,
    source_registry: ParticipantRegistry,
    source_dirty: DirtyTracker,
    wall: WallAnchor,
    destination: DestinationTransport,
    current_route: RouteHandle,
    next_route: RouteHandle,
}

impl Fixture {
    fn new() -> Self {
        let source_shard = ShardId::new(0);
        let destination_shard = ShardId::new(1);
        let wall = WallAnchor::new(SystemTime::UNIX_EPOCH, Instant::now());
        let mut keys = SlotMap::<TrackKey, ()>::with_key();
        let source_key = keys.insert(());
        let destination_key = keys.insert(());
        let current_route = RouteHandle::new(RouteId::new(destination_shard, 0), 0);
        let next_route = RouteHandle::new(RouteId::new(destination_shard, 1), 0);
        let (_, update_rx) = crate::shard_update::new_shard_update(destination_shard);
        let mut destination = ShardCore::new(destination_shard, 4, 2, wall, update_rx);
        destination.benchmark_install_track(destination_key);
        destination.benchmark_replace_route(
            RouteHandle::new(RouteId::new(destination_shard, 2), 0),
            current_route,
            destination_key,
        );

        let mut source = ShardRuntime::new(source_shard);
        source.apply_update_op(&ShardUpdateOp::InsertTrackRuntime {
            key: source_key,
            runtime: TrackRuntime::default(),
        });
        let source_plan = TrackPlan::new([], [current_route], None);

        Self {
            source,
            source_key,
            destination_key,
            source_plan,
            source_registry: ParticipantRegistry::new(source_shard, 4, 2),
            source_dirty: DirtyTracker::with_capacity(1),
            wall,
            destination: DestinationTransport {
                destination: RefCell::new(destination),
                null: NullTransport,
            },
            current_route,
            next_route,
        }
    }

    fn forward(&mut self) -> u128 {
        let packet = RtpPacket::default();
        let mut context = ForwardingContext {
            registry: &mut self.source_registry,
            dirty: &mut self.source_dirty,
            wall: &self.wall,
            router: &self.destination,
        };
        let start = CpuInstant::now();
        self.source.route_rtp_with_plan(
            self.source_key,
            Origin::Local,
            packet,
            &self.source_plan,
            &mut context,
        );
        black_box(start.elapsed().as_nanos())
    }

    fn churn(&mut self) {
        let old_route = self.current_route;
        let new_route = self.next_route;
        self.destination
            .destination
            .borrow_mut()
            .benchmark_replace_route(old_route, new_route, self.destination_key);
        self.current_route = new_route;
        self.next_route = old_route;
        self.source_plan = TrackPlan::new([], [new_route], None);
    }
}

pub fn run(samples: usize, churn_every: usize) -> Report {
    assert!(samples > 0);
    assert!(churn_every > 0);
    let steady = measure(samples, usize::MAX, churn_every);
    let churn = measure(samples, churn_every, churn_every);
    Report {
        steady,
        churn,
        samples,
        churn_every,
    }
}

fn measure(samples: usize, churn_every: usize, warmup: usize) -> Percentiles {
    let mut fixture = Fixture::new();
    for index in 0..warmup.min(100_000) {
        if churn_every != usize::MAX && index.checked_rem(churn_every) == Some(0) {
            fixture.churn();
        }
        black_box(fixture.forward());
    }
    let mut values = Vec::with_capacity(samples);
    for index in 0..samples {
        if churn_every != usize::MAX && index.checked_rem(churn_every) == Some(0) {
            fixture.churn();
        }
        values.push(fixture.forward());
    }
    percentiles(&mut values)
}

fn percentiles(values: &mut [u128]) -> Percentiles {
    values.sort_unstable();
    Percentiles {
        p50_ns: percentile(values, 5_000),
        p99_ns: percentile(values, 9_900),
        p999_ns: percentile(values, 9_990),
        p9999_ns: percentile(values, 9_999),
        max_ns: *values.last().unwrap_or(&0),
    }
}

fn percentile(values: &[u128], rank: usize) -> u128 {
    let index = values
        .len()
        .saturating_mul(rank)
        .div_ceil(10_000)
        .saturating_sub(1);
    values.get(index).copied().unwrap_or_default()
}

pub fn format_duration(ns: u128) -> Duration {
    Duration::from_nanos(u64::try_from(ns.min(u128::from(u64::MAX))).unwrap_or(u64::MAX))
}
