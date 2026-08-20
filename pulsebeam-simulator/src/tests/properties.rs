//! Invariants that must hold across randomly generated networks.
//!
//! The hand-written plans in `bwe.rs` each pin one scenario someone thought of. That is worth
//! having, but it only ever finds the failures we already imagined - every genuine surprise this
//! codebase has produced came from a condition nobody wrote a plan for. These generate the
//! condition instead and assert a claim that must hold whatever comes out.
//!
//! # What makes this possible
//!
//! Randomised simulation is only useful if a failure can be replayed exactly. That holds here:
//! the clock and RNG are the simulated ones (see `sim_clock.rs` / `sim_rand.rs`), each plan runs
//! in its own process, and a full run reproduces byte-identically. So a case that fails carries a
//! seed which reproduces it, and proptest can shrink toward the smallest network that still
//! breaks the claim rather than handing back the first mess it found.
//!
//! # Why the axes are coarse
//!
//! Every case is a full simulation costing seconds, so a run samples the space rather than
//! covering it. That makes the *size* of the space the thing that decides whether sampling means
//! anything. An axis generated at fine resolution - a headroom percentage over 150..400, a settle
//! time over 30..45s - spends most of its values on points that are indistinguishable to the code
//! under test, and a failure at "headroom 187%, capacity 2,347,221 bps" names a coordinate rather
//! than a condition.
//!
//! So each axis carries only values that cross a decision boundary: capacities sit on the ladder's
//! rungs and just either side of them, paths are named characters, settle time is whatever its
//! path needs. Two values that lead the code to the same decisions are the same test, however far
//! apart they look. `the_generated_space_is_small_enough_to_sample_and_large_enough_to_differ`
//! holds the result to a band, because both directions fail silently.
//!
//! # Why the invariants look weak
//!
//! Deliberately. A property is claimed over every network the generator can produce, including
//! badly congested ones, so it can only assert what is true of *all* of them. "The estimate
//! reaches 80% of capacity" is false on a link too small to carry the layer, and a property that
//! is false for good reasons teaches nothing. What survives is the small set of things a
//! congestion controller must never do regardless of conditions - and those are exactly the ones
//! that were being violated in production.

use super::common::{
    Content, Experience, LinkProfile, LinkReport, LocalNodeSim, MAX_TIME_TO_FIRST_FRAME,
    Participant, Room, Step, sim_seed,
};
use proptest::prelude::*;
use proptest::strategy::ValueTree;
use proptest::test_runner::{RngAlgorithm, TestCaseResult, TestRng, TestRunner};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[test]
/// Replays a failing run with `PULSEBEAM_SIM_SEED=<seed>` from the test output.
fn sharding_differential_preserves_the_media_contract() {
    for shards in [1, 2, 4, 8] {
        LocalNodeSim::new()
            .with_shards(shards)
            .with_room(
                Room::new("sharding-differential")
                    .with_participant(Participant::single_publisher("publisher"))
                    .with_participant(Participant::subscriber("subscriber")),
            )
            .run(vec![
                Step::Run {
                    description: "establish the same media contract",
                    duration: Duration::from_secs(8),
                },
                Step::CheckRxBytes {
                    description: "the subscriber is served at every shard count",
                    participant: "subscriber",
                    min_bytes: 1,
                },
            ]);
    }
}

/// The simulcast ladder every plan here publishes, and the single-layer screen share rate.
const LADDER_Q_BPS: u64 = 150_000;
const LADDER_H_BPS: u64 = 400_000;
const LADDER_F_BPS: u64 = 1_250_000;
const SCREENSHARE_BPS: u64 = 2_500_000;

/// Capacities placed on the ladder's decision boundaries.
///
/// Capacity used to be derived from demand times a generated headroom percentage, which made one
/// axis out of two independent things: what the network supplies and what the application asks
/// for. A failure could then not be attributed to either. These are absolute, and the relation to
/// demand emerges from the pair.
///
/// The values are the rungs themselves and the points just either side, because that is where an
/// allocator decides differently. A threshold bug lives at a boundary, so the boundaries are
/// generated rather than sampled around.
const CAPACITIES_BPS: [u64; 12] = [
    120_000,   // under the floor: not even one rung fits
    150_000,   // exactly the floor
    165_000,   // the floor with a little to spare
    400_000,   // exactly h
    550_000,   // h plus a floor: two streams, both at the bottom
    1_250_000, // exactly f
    1_400_000, // f plus a floor
    2_400_000, // just under a screen share
    2_500_000, // exactly a screen share
    2_650_000, // a screen share plus a floor
    4_000_000, // comfortable for a camera ladder and a co-tenant
    8_000_000, // ample for anything generated here
];

/// Link character, independent of capacity.
///
/// Reuses the harness vocabulary rather than inventing a parallel one: `LinkProfile` already
/// carries latency, jitter, burst loss, reordering, duplication and return-path impairment, and
/// leaves capacity to the caller. That separation is the reason character and capacity can be two
/// axes here instead of one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Path {
    Fiber,
    Wifi,
    Cellular,
}

impl Path {
    fn profile(self) -> LinkProfile {
        match self {
            Path::Fiber => LinkProfile::fiber(),
            Path::Wifi => LinkProfile::wifi(),
            Path::Cellular => LinkProfile::cellular(),
        }
    }

    /// How long this path needs before a claim about a settled stream is fair.
    ///
    /// Generated as 30..45s for every case before: fifteen values that all mean "settled", each
    /// costing simulated seconds to distinguish nothing. A path that stabilises quickly should
    /// not pay for the worst case.
    fn settle(self) -> Duration {
        match self {
            Path::Fiber => Duration::from_secs(12),
            Path::Wifi => Duration::from_secs(25),
            Path::Cellular => Duration::from_secs(35),
        }
    }

    fn strategy() -> impl Strategy<Value = Path> {
        prop_oneof![Just(Path::Fiber), Just(Path::Wifi), Just(Path::Cellular)]
    }
}

/// Whether the node is spread across shards.
///
/// Never generated before, and never named: which shard a participant lands on came out of the
/// `SO_REUSEPORT` 4-tuple hash, so any plan whose outcome depended on placement was a coin flip
/// dressed as an assertion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Placement {
    SingleShard,
    MultiShard,
}

impl Placement {
    fn shards(self) -> usize {
        match self {
            Placement::SingleShard => 1,
            Placement::MultiShard => 3,
        }
    }

    fn strategy() -> impl Strategy<Value = Placement> {
        prop_oneof![Just(Placement::SingleShard), Just(Placement::MultiShard)]
    }
}

/// A disturbance that arrives partway through, as distinct from the steady character of the path.
///
/// Burst loss and reordering used to live here too, which double-counted them: `Path::Wifi` and
/// `Path::Cellular` already lose in bursts and deliver out of order, so generating them again as
/// faults multiplied the space without reaching a new decision. What is left is the thing a path
/// character cannot express: an interruption with a beginning and an end, and the arrival and
/// departure of other participants. Timing rides on the variant rather than crossing it with a
/// separate axis - "during ramp vs settled" would double the whole space, and the placement that
/// matters differs per disturbance anyway.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    None,
    /// A total interruption partway through, which the connection survives. Bounded well under
    /// the point where ICE tears the session down; past that it is not an impairment but a lost
    /// session, which is a different failure with a different fix.
    Outage,
    /// Another publisher dies without signalling, mid-measurement.
    PeerCrash,
    /// Another publisher leaves cleanly and comes back.
    PeerRejoin,
    /// Several arrive and leave in quick succession.
    PeerStorm,
}

impl Fault {
    fn steps(self) -> Vec<Step> {
        match self {
            Fault::None => vec![],
            Fault::Outage => vec![
                Step::Partition {
                    description: "The viewer drops off the network",
                    from: "viewer",
                    to: "server",
                },
                Step::Run {
                    description: "Ride out the outage",
                    duration: Duration::from_secs(6),
                },
                Step::Repair {
                    description: "The network returns",
                    from: "viewer",
                    to: "server",
                },
                Step::Run {
                    description: "Re-establish flow",
                    duration: Duration::from_secs(40),
                },
            ],
            Fault::PeerCrash => vec![
                Step::Join {
                    description: "Another publisher joins",
                    participant: CHURNERS[0],
                },
                Step::Run {
                    description: "It publishes for a while",
                    duration: Duration::from_secs(6),
                },
                Step::AbruptExit {
                    description: "It dies without signalling",
                    participant: CHURNERS[0],
                },
                Step::Run {
                    description: "Carry on without it",
                    duration: Duration::from_secs(8),
                },
            ],
            Fault::PeerRejoin => vec![
                Step::Join {
                    description: "Another publisher joins",
                    participant: CHURNERS[0],
                },
                Step::Run {
                    description: "It publishes for a while",
                    duration: Duration::from_secs(6),
                },
                Step::Disconnect {
                    description: "It leaves cleanly",
                    participant: CHURNERS[0],
                },
                Step::Run {
                    description: "Gap before it returns",
                    duration: Duration::from_secs(4),
                },
                Step::Reconnect {
                    description: "It comes back",
                    participant: CHURNERS[0],
                },
                Step::Run {
                    description: "Settle again",
                    duration: Duration::from_secs(8),
                },
            ],
            Fault::PeerStorm => {
                let mut steps = Vec::new();
                for who in CHURNERS {
                    steps.push(Step::Join {
                        description: "A publisher arrives",
                        participant: who,
                    });
                    steps.push(Step::Run {
                        description: "Briefly active",
                        duration: Duration::from_secs(3),
                    });
                    steps.push(Step::AbruptExit {
                        description: "And is gone",
                        participant: who,
                    });
                }
                steps.push(Step::Run {
                    description: "Settle after the storm",
                    duration: Duration::from_secs(8),
                });
                steps
            }
        }
    }

    /// Publishers this fault needs standing by, disconnected until a step brings them in.
    fn churners(self) -> &'static [&'static str] {
        match self {
            Fault::None | Fault::Outage => &[],
            Fault::PeerCrash | Fault::PeerRejoin => &CHURNERS[..1],
            Fault::PeerStorm => &CHURNERS,
        }
    }
}

/// Publishers that come and go during a run.
///
/// Nobody subscribes to them: the viewer is a `manual_subscriber` bound to fixed targets, so the
/// route table, import lifecycle, quarantine expiry and refcounts churn underneath while every
/// claim stays a claim about `publisher` and `cotenant`. That is what keeps the properties
/// meaningful under churn rather than merely noisy - "a participant crashing does not disturb an
/// unrelated stream" is the assertion, and it is worth making.
const CHURNERS: [&str; 3] = ["churner1", "churner2", "churner3"];

/// What the application asks the SFU to carry. Independent of what the link supplies.
#[derive(Clone, Copy, Debug)]
struct Demand {
    /// Whether the camera publishes temporal layers (L1T3), so the SFU can shed *framerate*
    /// instead of dropping the stream.
    ///
    /// Restricted to the property that is about shedding rather than added to every scenario:
    /// an axis that cannot falsify a property is pure cost on it, and this one would multiply the
    /// whole space by 1.5 to be irrelevant almost everywhere.
    temporal: bool,
    /// Whether the publisher is a camera (steady) or a screen share (bursty, long idle gaps).
    /// The idle case is what puts the sender in ALR, which is where the interesting failures are.
    screenshare: bool,
    /// Height the viewer asks for, which decides how far up the ladder demand goes.
    target_height: u32,
    /// Whether a second publisher competes for the same viewer's link. Contention is a different
    /// axis from link quality: a controller can be perfectly well behaved about capacity and
    /// still let one stream starve to pay for another.
    contended: bool,
}

impl Demand {
    fn bps(&self) -> u64 {
        let publisher = if self.screenshare {
            SCREENSHARE_BPS
        } else {
            match self.target_height {
                180 => LADDER_Q_BPS,
                360 => LADDER_H_BPS,
                _ => LADDER_F_BPS,
            }
        };
        publisher + if self.contended { LADDER_Q_BPS } else { 0 }
    }

    fn any() -> impl Strategy<Value = Demand> {
        (any::<bool>(), 0usize..3, any::<bool>()).prop_map(
            |(screenshare, height_idx, contended)| Demand {
                temporal: false,
                screenshare,
                target_height: [180, 360, 720][height_idx],
                contended,
            },
        )
    }

    /// A second stream competing for the same viewer's link.
    fn contended() -> impl Strategy<Value = Demand> {
        (any::<bool>(), 0usize..3).prop_map(|(screenshare, height_idx)| Demand {
            temporal: false,
            screenshare,
            target_height: [180, 360, 720][height_idx],
            contended: true,
        })
    }

    /// A camera publishing temporal layers, so the SFU has framerate to shed.
    ///
    /// Single spatial rung on purpose - `with_temporal_dd` decorates one encoding - so the only
    /// way to fit a tight link is to drop temporal layers. That is the graceful degradation a weak
    /// link should get, and the case the whole "renders blank instead of degrading" complaint is
    /// about.
    fn temporal_camera() -> impl Strategy<Value = Demand> {
        (any::<bool>()).prop_map(|contended| Demand {
            temporal: true,
            screenshare: false,
            target_height: 180,
            contended,
        })
    }

    /// Contended, a camera, and asked for above the co-tenant's 180p floor.
    ///
    /// `the_stream_asked_for_more_is_not_served_less` stated this as a `prop_assume!` on the far
    /// side of the simulation, so two thirds of its cases were generated, simulated in full, and
    /// then discarded - which is why it ran at 132s against 44-65s for every other property.
    fn contended_camera_above_the_floor() -> impl Strategy<Value = Demand> {
        (0usize..2).prop_map(|height_idx| Demand {
            temporal: false,
            screenshare: false,
            target_height: [360, 720][height_idx],
            contended: true,
        })
    }
}

/// The relation a property needs between what the link supplies and what is asked of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Budget {
    /// Room to spare, so falling short is never the link's fault.
    Ample,
    /// Enough for everything, but not by much. Reusing `Ample` for the contention properties made
    /// them nearly vacuous: with capacity at several times demand nothing has to be given up, so
    /// an allocator that starves a co-tenant the moment budget is tight would pass.
    Tight,
    /// Not enough for everything, so something has to be given up.
    ///
    /// `Tight` cannot force that: it admits only capacities at or above demand, so the allocator
    /// can serve every stream and merely lowers quality. A property about *shedding* needs a link
    /// that cannot carry the whole demand, or its precondition is never met and the run is spent
    /// simulating cases it then discards.
    Starved,
}

impl Budget {
    fn admits(self, capacity_bps: u64, demand_bps: u64) -> bool {
        match self {
            Budget::Ample => capacity_bps >= demand_bps.saturating_mul(2),
            Budget::Tight => {
                capacity_bps >= demand_bps && capacity_bps < demand_bps.saturating_mul(3) / 2
            }
            // Below demand, but not so far below that nothing can be served at all - a link that
            // carries no stream tests the floor, not the choice of what to drop.
            Budget::Starved => {
                capacity_bps < demand_bps && capacity_bps.saturating_mul(4) >= demand_bps
            }
        }
    }
}

/// A generated network and the load offered over it.
#[derive(Clone, Debug)]
struct Scenario {
    demand: Demand,
    path: Path,
    placement: Placement,
    fault: Fault,
    capacity_bps: u64,
}

/// Scenarios whose capacity stands in the wanted relation to their demand, decided *before*
/// anything is simulated.
///
/// The previous shape ran the plan and then called `prop_assume!` on the result, so a case
/// rejected for the wrong height or the wrong headroom had already cost a full simulation.
/// The rule now: an assumption about the input belongs in the generator, an assumption about the
/// outcome has to stay in the body.
fn scenarios(
    demand: impl Strategy<Value = Demand>,
    budget: Budget,
    faults: &'static [Fault],
) -> impl Strategy<Value = Scenario> {
    debug_assert!(
        !faults.is_empty(),
        "a property must admit some fault, even None"
    );
    (
        demand,
        Path::strategy(),
        Placement::strategy(),
        proptest::sample::select(faults),
    )
        .prop_flat_map(move |(demand, path, placement, fault)| {
            let wanted = demand.bps();
            let admissible: Vec<u64> = CAPACITIES_BPS
                .into_iter()
                .filter(|&c| budget.admits(c, wanted))
                .collect();
            debug_assert!(
                !admissible.is_empty(),
                "no capacity on the ladder satisfies {budget:?} for {wanted} bps of demand, so \
                 this property would generate nothing and pass vacuously"
            );
            (
                Just(demand),
                Just(path),
                Just(placement),
                Just(fault),
                proptest::sample::select(admissible),
            )
        })
        .prop_map(|(demand, path, placement, fault, capacity_bps)| Scenario {
            demand,
            path,
            placement,
            fault,
            capacity_bps,
        })
}

/// Cases for a property whose links have room to spare. Those cases are cheap:
/// the link is not saturated, so the simulation is not spending its time on
/// queueing and retransmission.
const SPACIOUS: u32 = 20;

/// Cases for a property that runs its link at the edge of demand.
///
/// A saturated link costs several times what a spacious one does to simulate,
/// and the `Tight` space is the smaller of the two, so fewer cases sample a
/// comparable fraction of it. Sampling is proportional to what a case buys,
/// not uniform across properties that cost wildly different amounts.
const SATURATED: u32 = 12;

const NO_FAULT: &[Fault] = &[Fault::None];

/// Repeated rather than weighted because `select` is uniform over the slice. Roughly half of all
/// cases stay undisturbed, matching how often anything actually goes wrong.
const ANY_FAULT: &[Fault] = &[
    Fault::None,
    Fault::None,
    Fault::None,
    Fault::None,
    Fault::Outage,
    Fault::PeerCrash,
    Fault::PeerRejoin,
    Fault::PeerStorm,
];

/// Churn without a path interruption, for claims an outage would legitimately break.
///
/// A participant crashing is not a reason for an unrelated stream to shed a layer or for its
/// estimate to collapse, so these properties keep asserting through it. An outage is such a
/// reason, so it stays out.
const PEER_CHURN: &[Fault] = &[
    Fault::None,
    Fault::None,
    Fault::PeerCrash,
    Fault::PeerRejoin,
    Fault::PeerStorm,
];

impl Scenario {
    /// What this scenario's publisher is sending, which decides what watchable means for it.
    fn content(&self) -> Content {
        if self.demand.screenshare {
            Content::Static
        } else {
            Content::Motion
        }
    }

    /// A subnet derived from the scenario, so a replayed case runs the network it originally did
    /// rather than whichever one its position in the sample happened to give it.
    fn subnet(&self, namespace: &str) -> u8 {
        debug_assert!(!namespace.is_empty());
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        namespace.hash(&mut hasher);
        self.capacity_bps.hash(&mut hasher);
        self.demand.screenshare.hash(&mut hasher);
        self.demand.temporal.hash(&mut hasher);
        self.demand.target_height.hash(&mut hasher);
        self.demand.contended.hash(&mut hasher);
        format!("{:?}", self.path).hash(&mut hasher);
        format!("{:?}", self.placement).hash(&mut hasher);
        format!("{:?}", self.fault).hash(&mut hasher);
        (hasher.finish() % 200) as u8
    }

    fn run(&self, namespace: &str) -> LinkReport {
        let publisher = if self.demand.screenshare {
            Participant::screensharer("publisher")
        } else if self.demand.temporal {
            Participant::publisher("publisher", &["q"]).with_temporal_dd(3)
        } else {
            Participant::publisher("publisher", &["q", "h", "f"])
        };
        // The co-tenant is always asked for at its lowest rung. The claim being set up is that a
        // stream wanting very little is not starved outright, which is a far weaker thing to ask
        // than that both get what they want.
        let targets: &'static [(&'static str, u32)] =
            match (self.demand.target_height, self.demand.contended) {
                (180, false) => &[("publisher", 180)],
                (360, false) => &[("publisher", 360)],
                (_, false) => &[("publisher", 720)],
                (180, true) => &[("publisher", 180), ("cotenant", 180)],
                (360, true) => &[("publisher", 360), ("cotenant", 180)],
                (_, true) => &[("publisher", 720), ("cotenant", 180)],
            };

        let mut plan = vec![
            Step::Run {
                description: "Establish connection and discover the track",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeTo {
                description: "Viewer subscribes",
                participant: "viewer",
                targets,
            },
            Step::Run {
                description: "Settle",
                duration: self.path.settle(),
            },
        ];
        plan.extend(self.fault.steps());
        // A fresh window last, so every claim describes the state the plan arrives at rather
        // than the disturbance on the way there.
        plan.push(Step::Run {
            description: "Measurement window",
            duration: Duration::from_secs(30),
        });

        let mut room = Room::new("room1").with_participant(publisher);
        if self.demand.contended {
            room = room.with_participant(Participant::publisher("cotenant", &["q", "h", "f"]));
        }
        for who in self.fault.churners() {
            room = room.with_participant(Participant::single_publisher(who).starts_disconnected());
        }
        let slots = if self.demand.contended { 2 } else { 1 };
        let reports = LocalNodeSim::new()
            .with_subnet(self.subnet(namespace))
            .with_link(self.path.profile())
            .with_bandwidth(self.capacity_bps)
            .with_shards(self.placement.shards())
            .with_room(room.with_participant(Participant::manual_subscriber("viewer", slots)))
            .run_collecting(plan);
        reports
            .get("viewer")
            .cloned()
            .expect("the viewer should have been measured")
    }

    /// The same scenario with two viewers, at a chosen shard count, reporting on both.
    ///
    /// Two because one cannot see the failures that matter here: a fanout serving only the first
    /// subscriber on a shard looks perfect to a plan with one subscriber, and that is exactly how
    /// a data-topic drop stayed green.
    fn run_two_viewers(
        &self,
        namespace: &str,
        placement: Placement,
    ) -> BTreeMap<String, DeliverySignature> {
        let publisher = if self.demand.screenshare {
            Participant::screensharer("publisher")
        } else {
            Participant::publisher("publisher", &["q", "h", "f"])
        };
        const TARGETS: &[(&str, u32)] = &[("publisher", 720)];
        let plan = vec![
            Step::Run {
                description: "Establish connections and discover the track",
                duration: Duration::from_secs(5),
            },
            Step::SubscribeTo {
                description: "First viewer subscribes",
                participant: "viewer",
                targets: TARGETS,
            },
            Step::SubscribeTo {
                description: "Second viewer subscribes",
                participant: "viewer2",
                targets: TARGETS,
            },
            Step::Run {
                description: "Settle",
                duration: self.path.settle(),
            },
            Step::Run {
                description: "Measurement window",
                duration: Duration::from_secs(20),
            },
        ];

        let room = Room::new("room1")
            .with_participant(publisher)
            .with_participant(Participant::manual_subscriber("viewer", 1))
            .with_participant(Participant::manual_subscriber("viewer2", 1));

        LocalNodeSim::new()
            .with_subnet(self.subnet(namespace))
            .with_link(self.path.profile())
            .with_bandwidth(self.capacity_bps)
            .with_shards(placement.shards())
            .with_room(room)
            .run_collecting(plan)
            .into_iter()
            .filter(|(name, _)| name.starts_with("viewer"))
            .map(|(name, report)| (name.to_owned(), DeliverySignature::of(&report)))
            .collect()
    }
}

/// What a subscriber observably got, coarse enough to survive a different packet schedule.
///
/// Deliberately not bitrates or byte counts: two shard layouts schedule packets differently, so
/// those differ by a little every run and comparing them would report noise. What must not differ
/// is *whether a subscriber was served at all* and *which publishers reached it* - the questions
/// both cross-shard defects found so far got wrong.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct DeliverySignature {
    received_media: bool,
    served_origins: Vec<String>,
}

impl DeliverySignature {
    fn of(report: &LinkReport) -> Self {
        let mut served_origins: Vec<String> = report
            .forwarded_quality
            .iter()
            .filter(|(_, quality)| **quality > 0)
            .map(|(origin, _)| (*origin).to_owned())
            .collect();
        served_origins.sort();
        Self {
            received_media: report.received_bytes > 0,
            served_origins,
        }
    }
}

fn config(cases: u32) -> ProptestConfig {
    ProptestConfig {
        // Each case is a full simulation. The count is worth more than it was: cases are no
        // longer discarded after being simulated, and the space they sample is small enough that
        // this many is a real sample of it.
        cases,
        max_shrink_iters: 8,
        // Every case here is a full simulation, and these properties `prop_assume!` *after*
        // running one, so a rejected case costs as much as an accepted one. proptest's default of
        // 1024 therefore lets a precondition that has stopped being reachable spend half an hour
        // before saying so - which reads as a hung suite rather than a failing one, and did.
        //
        // The assumes left here only ask that a case produced data at all, which should hold
        // almost always, so this is a backstop rather than a working budget: the two properties
        // that rejected selectively now skip or starve by construction instead. Sixteen per wanted
        // case leaves room for a genuinely unlucky run while keeping an unreachable precondition
        // distinguishable from a hang.
        max_global_rejects: cases.saturating_mul(16),
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::WithSource("regressions"),
        )),
        ..ProptestConfig::default()
    }
}

/// The generated space stays small enough that one run samples it meaningfully and large enough
/// that two seeds still disagree.
///
/// Both ends fail silently. An axis added at wide resolution takes the space back to the hundreds
/// of thousands, where a run samples a fraction of a percent and a failure cannot be attributed to
/// anything; over-tightening takes it to a handful, where every seed runs the same cases and
/// sweeping proves nothing. Neither shows up as a failure anywhere else.
///
/// What the bound is guarding against is *resolution creep* - a range where a set of named values
/// belongs. Growth from a genuinely new axis is how this suite is supposed to get better, so
/// raising the ceiling to admit one is legitimate; widening it to fit a re-introduced continuous
/// parameter is not. Adding lifecycle churn as five discrete disturbances took the widest property
/// from 504 to 1260, which is that first kind.
#[test]
fn the_generated_space_is_small_enough_to_sample_and_large_enough_to_differ() {
    fn size(budget: Budget, faults: &[Fault]) -> usize {
        let mut total = 0usize;
        for screenshare in [false, true] {
            for target_height in [180, 360, 720] {
                for contended in [false, true] {
                    let demand = Demand {
                        temporal: false,
                        screenshare,
                        target_height,
                        contended,
                    };
                    let capacities = CAPACITIES_BPS
                        .into_iter()
                        .filter(|&c| budget.admits(c, demand.bps()))
                        .count();
                    let distinct_faults = {
                        let mut seen: Vec<Fault> = Vec::new();
                        for f in faults {
                            if !seen.contains(f) {
                                seen.push(*f);
                            }
                        }
                        seen.len()
                    };
                    total += capacities * 3 * 2 * distinct_faults;
                }
            }
        }
        total
    }

    for budget in [Budget::Ample, Budget::Tight] {
        for faults in [NO_FAULT, PEER_CHURN, ANY_FAULT] {
            let size = size(budget, faults);
            assert!(
                (60..=1400).contains(&size),
                "{budget:?} with {} fault(s) generates {size} distinct scenarios; outside \
                 60..=1400 the sample is either too thin to attribute a failure or too small \
                 for two seeds to disagree. Check whether the growth came from a new named axis, \
                 which is fine, or from a range creeping back in, which is not",
                faults.len()
            );
        }
    }
}

/// The seed must reach the *generator*, not only the network.
///
/// `check` exists because proptest otherwise seeds itself from the OS. If that regressed, every
/// property below would keep passing while the replay command they print on failure quietly
/// became a lie — the worst possible failure mode for a randomised suite, because it is invisible
/// until someone needs it.
///
/// The network-level counterpart lives in `bwe.rs`
/// (`a_different_seed_is_a_different_network_test`). This one asserts at the generator so it costs
/// no simulation.
#[test]
fn the_seed_selects_the_generated_scenarios() {
    fn sample(seed: u64) -> Vec<String> {
        let mut rng_seed = [0u8; 32];
        rng_seed[..8].copy_from_slice(&seed.to_le_bytes());
        let mut runner = TestRunner::new_with_rng(
            config(SPACIOUS),
            TestRng::from_seed(RngAlgorithm::ChaCha, &rng_seed),
        );
        let strategy = scenarios(Demand::any(), Budget::Ample, ANY_FAULT);
        (0..16)
            .map(|_| format!("{:?}", strategy.new_tree(&mut runner).unwrap().current()))
            .collect()
    }

    assert_eq!(
        sample(1),
        sample(1),
        "the same seed generated different scenarios, so a reported failure cannot be replayed"
    );
    assert_ne!(
        sample(1),
        sample(2),
        "two seeds generated identical scenarios, so the seed is not reaching the generator and \
         sweeping it proves nothing"
    );
}

/// Run `strategy` under this process's simulation seed.
///
/// proptest draws its generator entropy from the OS by default, which would leave a generated
/// failure unreproducible: the plan seed replays the network but not the scenario that ran on it.
/// Seeding the runner from `sim_seed()` makes the seed the whole input, so the command printed
/// below genuinely reproduces the run.
fn check<S>(cases: u32, strategy: S, test: impl Fn(S::Value) -> TestCaseResult)
where
    S: Strategy,
    S::Value: std::fmt::Debug,
{
    let seed = sim_seed();
    let mut rng_seed = [0u8; 32];
    rng_seed[..8].copy_from_slice(&seed.to_le_bytes());

    let mut runner = TestRunner::new_with_rng(
        config(cases),
        TestRng::from_seed(RngAlgorithm::ChaCha, &rng_seed),
    );

    if let Err(err) = runner.run(&strategy, test) {
        panic!("{err}\n\nreplay this exact run with:\n    make test-sim-seed SEED={seed}\n");
    }
}

/// How the node is sharded must not change who gets served.
///
/// A differential property: rather than asserting what the right outcome is, it runs the same
/// scenario in two configurations that must agree, and reports the disagreement. That makes it the
/// one oracle here immune to the problem every threshold has - there is no number to pick, no
/// argument about what a correct controller should achieve, and no way for it to be quietly wrong
/// in the way `QueueingDelayBelow` was.
///
/// Sharding is a placement decision. `SO_REUSEPORT` hashes a 4-tuple, so which worker owns a
/// participant is not something the application chose and must not be something a subscriber can
/// notice. Every cross-shard defect found so far is a difference this would have reported
/// immediately and unambiguously: a data topic reaching only the first subscriber on a shard, and
/// a keyframe path that went nowhere for a shard that joined the room late.
///
/// Bitrates are excluded on purpose - see [`DeliverySignature`]. Only the questions those bugs got
/// wrong are compared.
#[test]
fn sharding_does_not_change_who_is_served() {
    check(
        SPACIOUS,
        scenarios(Demand::any(), Budget::Ample, NO_FAULT),
        |scenario| {
            let single = scenario.run_two_viewers("shard_equiv", Placement::SingleShard);
            let multi = scenario.run_two_viewers("shard_equiv", Placement::MultiShard);

            prop_assert!(
                single.values().any(|s| s.received_media),
                "neither viewer received anything even on one shard, so this case proves \
                 nothing about sharding ({scenario:?}, {single:?})"
            );
            prop_assert_eq!(
                &single,
                &multi,
                "spreading the node across shards changed who was served. A subscriber cannot \
                 tell which worker owns it, so this is the SFU losing a stream to its own \
                 internal placement ({:?})",
                scenario
            );
            Ok(())
        },
    );
}

// A liveness gate belongs here and is deliberately absent for now.
//
// The measurement exists - `Qoe::longest_freeze` and `freeze_ratio`, both on the scoreboard - and
// it found real freezes: 10.4s on cellular under peer churn, 53s in one authored plan, several
// viewers 95% frozen at under 2 fps. Those are defects to fix, not thresholds to tune, so the gate
// lands once they are. Choosing a bound large enough to pass today would leave a green test
// asserting nothing anyone cares about.

/// A stream with framerate to shed must shed it, not stop.
///
/// The graceful degradation a weak link should get, and the thing the original complaint was
/// about: on two bars of signal a tile went blank when a quarter-resolution stream at reduced
/// framerate would have fit. A publisher with temporal layers gives the SFU somewhere to go
/// between "full quality" and "nothing" - dropping the top temporal layer halves the framerate
/// and the picture keeps moving.
///
/// The link is tight, so something has to give. What may not give is the picture: `Watchable`
/// still demands the framerate floor, so this fails both if the stream stops and if it degrades
/// into a slideshow.
#[test]
fn a_stream_with_layers_to_shed_keeps_moving() {
    check(
        SATURATED,
        scenarios(Demand::temporal_camera(), Budget::Tight, NO_FAULT),
        |scenario| {
            let report = scenario.run("temporal_shedding");
            prop_assume!(report.samples > 0 && report.received_bytes > 0);

            // Gates on the stream surviving. The full claim - that it stays *watchable* by
            // shedding a temporal layer - does not hold yet, and it is a defect rather than a
            // wrong bar: at 150 kbps, which cannot carry the 150 kbps rung once headers are
            // counted, the SFU forwards ~24 fps and the viewer spends 62% of the session frozen
            // instead of dropping to the ~7.5 fps base layer. The layers are advertised (VLA
            // reaches the SFU through `with_temporal_dd`) and `DecodeTargetSelection` exists to
            // act on them, so the machinery is there and is not being used. Tighten to
            // `Experience::Watchable` once it is.
            prop_assert_ne!(
                report.qoe.experience(scenario.content()),
                Experience::Blank,
                "a publisher with temporal layers had framerate to give up and the viewer got \
                 nothing at all ({:?}, {:?})",
                scenario,
                report.qoe,
            );
            Ok(())
        },
    );
}

/// A stream the SFU stops forwarding must be *signalled* as paused, not merely go quiet.
///
/// This is the complaint that started the QoE work, stated as a property: on a weak link a tile
/// renders blank, when a placeholder saying "paused" would tell the viewer what is happening. From
/// the media alone the two are identical - a paused stream and a dead connection are both an
/// absence of packets - so the client can only draw the right thing if it was told.
///
/// The generator runs *starved* links, so the allocator genuinely cannot carry everything and has
/// to drop something. Whenever it does, the viewer must have heard about it.
///
/// It ran `Budget::Tight` until a keyframe defect was fixed, and that was never right: `Tight`
/// admits only capacities at or above demand, so nothing has to be shed and the allocator merely
/// lowers quality. The cases it was accepting were streams stuck at quality 0 because they had
/// never received a keyframe - broken, not shed - so the property was being fed by the bug rather
/// than by the behaviour it describes.
#[test]
fn a_stream_the_sfu_sheds_is_signalled_not_just_silent() {
    check(
        SATURATED,
        scenarios(Demand::contended(), Budget::Starved, NO_FAULT),
        |scenario| {
            let report = scenario.run("pause_signalled");
            prop_assume!(report.samples > 0);

            // Only meaningful where something was actually shed.
            let shed: Vec<&str> = report
                .forwarded_quality
                .iter()
                .filter(|(_, quality)| **quality == 0)
                .map(|(origin, _)| *origin)
                .collect();
            prop_assume!(!shed.is_empty());

            prop_assert!(
                !report.signalled_paused.is_empty(),
                "the SFU stopped forwarding {shed:?} and the viewer was never told. A client \
                 cannot tell that from a dead connection, so it renders a blank tile where a \
                 placeholder belongs ({:?})",
                scenario,
            );
            Ok(())
        },
    );
}

/// Bytes arriving is not a picture. What arrives must be decodable.
///
/// Every other claim here is about bytes, bitrates and layer indices, and a viewer cannot see any
/// of those. A stream can arrive at the right rate, on the right layer, with the right byte total,
/// and still render nothing: a keyframe without SPS+PPS in the same picture is not decodable, so
/// the decoder emits nothing and the tile stays blank while every byte-level measure looks
/// healthy. That is the shape of "it works in the numbers and the user sees black", and until the
/// report carried frame-level facts no generated property could tell the two apart.
#[test]
fn what_arrives_can_actually_be_decoded() {
    check(
        SPACIOUS,
        scenarios(Demand::any(), Budget::Ample, PEER_CHURN),
        |scenario| {
            let report = scenario.run("decodable");
            prop_assume!(report.samples > 0 && report.received_bytes > 0);

            // Gates on the part of the bar the product meets; the scoreboard carries the rest.
            // Only the freeze clause is still held back, and only because freezes past 8s under
            // churn are real defects rather than a wrong threshold - gating on it now would mean
            // a permanently red suite or a bar quietly lowered until it passed.
            prop_assert_ne!(
                report.qoe.experience(scenario.content()),
                Experience::Blank,
                "media arrived and nothing rendered: the viewer saw a blank tile while every \
                 byte-level measure looked healthy ({:?}, {:?})",
                scenario,
                report.qoe,
            );
            prop_assert_eq!(
                report.qoe.undecodable_keyframes,
                0,
                "keyframes arrived without their parameter sets, so the decoder could not render \
                 them. Each is a stretch of blank picture no bitrate or layer figure would show \
                 ({:?})",
                scenario,
            );
            // Motion only, for the same reason the framerate floor and the freeze bounds are.
            // A still screen share has nothing to send, so the wait measures the source deciding
            // to produce a frame rather than the SFU delivering one - a 7.4s first frame from a
            // static share on an 8 Mbps link is correct behaviour, not a defect.
            if let (Content::Motion, Some(ttff)) =
                (scenario.content(), report.qoe.time_to_first_frame)
            {
                prop_assert!(
                    ttff <= MAX_TIME_TO_FIRST_FRAME,
                    "the viewer waited {ttff:?} to see anything after subscribing. A blank tile \
                     for that long is indistinguishable from a broken call ({:?})",
                    scenario,
                );
            }
            Ok(())
        },
    );
}

/// A link that can carry the bottom rung must show a picture, not nothing.
///
/// The real complaint this comes from: on a weak mobile link a tile renders blank, when a quarter
/// resolution stream would have fit. Degrading is correct; going blank is not. The generator makes
/// the case unambiguous by only offering links that can carry the floor, so a viewer seeing
/// nothing is never the link's fault.
#[test]
fn a_link_that_can_carry_the_floor_renders_something() {
    check(
        SATURATED,
        scenarios(Demand::any(), Budget::Tight, PEER_CHURN),
        |scenario| {
            prop_assume!(scenario.capacity_bps >= LADDER_Q_BPS);
            let report = scenario.run("renders_floor");
            prop_assume!(report.samples > 0);

            prop_assert_ne!(
                report.qoe.experience(scenario.content()),
                Experience::Blank,
                "nothing rendered on a {} bps link, which carries the {} bps bottom rung several \
                 times over. Shedding to the smallest layer is the right answer to a tight link; \
                 a blank tile is not ({:?}, {:?})",
                scenario.capacity_bps,
                LADDER_Q_BPS,
                scenario,
                report.qoe,
            );
            Ok(())
        },
    );
}

/// On a link with room to spare, a subscribed stream must actually be delivered.
///
/// The weakest possible statement of what an SFU is for, and the one production was violating:
/// the screen share was subscribed, the link was fine, and nothing arrived. It holds for every
/// network here by construction - capacity is at least twice demand - so a failure is never the
/// link's fault.
#[test]
fn a_subscribed_stream_is_delivered_on_a_link_with_room() {
    check(
        SPACIOUS,
        scenarios(Demand::any(), Budget::Ample, ANY_FAULT),
        |scenario| {
            let report = scenario.run("subscribed_stream");
            prop_assert!(
                report.samples > 0,
                "no allocation passes recorded; the plan never exercised the viewer, so every \
                 other claim here would pass vacuously ({scenario:?})"
            );
            prop_assert!(
                report.received_bytes > 0,
                "nothing was delivered over 30s on a {} bps link carrying {} bps of demand \
                 ({scenario:?}, report {report:?})",
                scenario.capacity_bps,
                scenario.demand.bps(),
            );
            Ok(())
        },
    );
}

/// The estimate must not fall below what the application asked for, on a link that can supply it.
///
/// This is the production failure stated as a property. The estimate is not required to find the
/// capacity - it cannot, when the application is not using it - only to keep up with demand it is
/// being asked to carry, on a link demonstrably able to carry it.
#[test]
fn the_estimate_keeps_up_with_demand_it_can_afford() {
    check(
        SPACIOUS,
        scenarios(Demand::any(), Budget::Ample, ANY_FAULT),
        |scenario| {
            let report = scenario.run("demand");
            prop_assume!(report.samples > 0 && report.received_bytes > 0);

            let need = report.need_bps();
            prop_assume!(need > 0);

            let got = report.estimate_last_bps as f64 / need as f64 * 100.0;
            prop_assert!(
                got >= 60.0,
                "estimate ended at {} bps against a need of {need} bps ({got:.0}%) on a {} bps \
                 link, having dropped {:.1}% from its peak with {:.2}% congestion loss \
                 ({scenario:?})",
                report.estimate_last_bps,
                scenario.capacity_bps,
                report.worst_drawdown_percent,
                report.congestion_loss_percent(),
            );
            Ok(())
        },
    );
}

/// A controller must not sustain congestion loss on a link it is not filling.
///
/// Overuse is defined by the bottleneck's own tail-drop, not by a bitrate anyone chose, so this
/// holds for any capacity: if the link is not being filled, packets have no business being
/// dropped for congestion.
#[test]
fn an_underused_link_is_not_driven_into_loss() {
    check(
        SPACIOUS,
        scenarios(Demand::any(), Budget::Ample, ANY_FAULT),
        |scenario| {
            let report = scenario.run("underused_link");
            prop_assume!(report.samples > 0 && report.received_bytes > 0);
            let Some(utilisation) = report.utilisation_percent() else {
                return Ok(());
            };
            prop_assume!(utilisation < 50.0);

            prop_assert!(
                report.congestion_loss_percent() < 1.0,
                "{:.2}% congestion loss while using only {utilisation:.1}% of the link \
                 ({} of {} packets, queue peaked at {:?}) ({scenario:?})",
                report.congestion_loss_percent(),
                report.congestion_drops,
                report.delivered_packets + report.congestion_drops,
                report.max_backlog,
            );
            Ok(())
        },
    );
}

/// A stream asking for the least the ladder offers must not be starved to pay for another.
///
/// The production report was "the allocator doesn't think there is enough for the camera", with
/// the camera paused while a screen share ran. The co-tenant asks for the bottom rung only, so
/// there is no capacity argument available: a stream left paused here was not priced out, it was
/// overlooked.
///
/// Stated as "not paused" rather than "gets its fair share". What fairness means between a screen
/// share and a camera is a policy question with defensible answers either way; being dropped
/// entirely is not one of them.
#[test]
fn a_cheap_co_tenant_is_not_starved() {
    // Counted rather than assumed. The condition below is a runtime outcome, so it cannot be moved
    // into the generator, and rejecting on it after the fact throws away a whole simulation -
    // which made this the slowest and least predictable property here, its cost set by how often
    // the estimate happened to land above demand. Skipping the assertion instead keeps the case,
    // and the tally makes the one risk of doing so visible: a property that silently stopped
    // asserting anything would otherwise still be green.
    let exercised = AtomicUsize::new(0);

    check(
        SATURATED,
        scenarios(Demand::contended(), Budget::Tight, ANY_FAULT),
        |scenario| {
            let report = scenario.run("cheap_cotenant");
            prop_assume!(report.samples > 0 && report.received_bytes > 0);

            // Conditioned on the *estimate* covering demand, not the link. The allocator can only
            // spend what it is given, so a starved co-tenant on a healthy link with a low estimate
            // is a bandwidth-estimation failure and belongs to the property above - conflating the
            // two here would leave neither diagnosable.
            if report.estimate_last_bps <= report.demand_last_bps {
                return Ok(());
            }
            exercised.fetch_add(1, Ordering::Relaxed);

            let quality = report.forwarded_quality.get("cotenant").copied();
            prop_assert!(
                quality.is_some_and(|q| q > 0),
                "the co-tenant was left at {quality:?} (0 = paused) on a {} bps link that covers \
                 the full {} bps of demand, so nothing was priced out. The other stream ended at \
                 {:?}. ({scenario:?}, estimate {} bps, {:.2}% congestion loss)",
                scenario.capacity_bps,
                report.demand_last_bps,
                report.forwarded_quality.get("publisher"),
                report.estimate_last_bps,
                report.congestion_loss_percent(),
            );
            Ok(())
        },
    );

    assert!(
        exercised.load(Ordering::Relaxed) > 0,
        "no generated case reached the assertion: every one ended with the estimate at or below \
         demand, so this property asserted nothing and passed vacuously"
    );
}

/// A settled stream must not oscillate between layers, whatever the network did to get there.
///
/// Every other figure a plan reports is an average or an endpoint, and a stream flipping between
/// two layers looks healthy in all of them: right final layer, right byte count, right estimate.
/// Only a reversal count sees it, and only a reversal count can be asserted while the link is
/// still ramping - climbing q to h to f is correct, turning round is not.
///
/// Faults are excluded rather than assumed away: an outage is a genuine reason to shed and then
/// climb back, so the reversal it causes is correct behaviour rather than instability.
#[test]
fn a_forwarded_stream_does_not_oscillate() {
    check(
        SPACIOUS,
        scenarios(Demand::any(), Budget::Ample, PEER_CHURN),
        |scenario| {
            let report = scenario.run("no_oscillation");
            prop_assume!(report.samples > 0 && report.received_bytes > 0);

            for (publisher, reversals) in &report.quality_reversals {
                prop_assert!(
                    *reversals <= 2,
                    "{publisher}'s layer reversed direction {reversals} times (changing {:?} times \
                     in total) on a {} bps link. Climbing is fine; turning round repeatedly means \
                     the stream never settles, and every switch costs a keyframe ({scenario:?}, \
                     estimate {} bps, drawdown {:.1}%)",
                    report.quality_changes.get(publisher),
                    scenario.capacity_bps,
                    report.estimate_last_bps,
                    report.worst_drawdown_percent,
                );
            }
            Ok(())
        },
    );
}

/// The estimate must not collapse on a link that never changed.
///
/// Capacity here is fixed for the life of the plan, so a deep fall from peak is not the link being
/// withdrawn - it is the controller mistaking something else for congestion. That is exactly what
/// a throughput sample taken while the sender was idle used to do: a 2.6 Mbps estimate went to
/// 67 kbps in one step with no packet loss at all, and the allocator could only offer what the
/// collapsed estimate allowed, so it could not climb back out.
#[test]
fn a_fixed_link_does_not_collapse_the_estimate() {
    check(
        SPACIOUS,
        scenarios(Demand::any(), Budget::Ample, PEER_CHURN),
        |scenario| {
            let report = scenario.run("no_collapse");
            prop_assume!(report.samples > 0 && report.received_bytes > 0);
            prop_assume!(report.capacity_fixed);

            prop_assert!(
                report.worst_drawdown_percent < 75.0,
                "the estimate fell {:.1}% from its peak on a link whose capacity never moved, \
                 ending at {} bps against {} bps of demand with {:.2}% congestion loss. A fixed \
                 link gives nothing to back off from ({scenario:?})",
                report.worst_drawdown_percent,
                report.estimate_last_bps,
                report.demand_last_bps,
                report.congestion_loss_percent(),
            );
            Ok(())
        },
    );
}

/// Under contention, the stream asked for at a higher rung must not end below the one asked for at
/// the lowest.
///
/// The co-tenant always requests 180p and the publisher requests more, so this is the weakest
/// possible statement of "what the viewer asked for is what decides who gets the bandwidth". It
/// fails on a priority inversion - which the allocator had, with a low-priority screen share
/// holding a floor that preempted a focused camera's target.
#[test]
fn the_stream_asked_for_more_is_not_served_less() {
    check(
        SATURATED,
        scenarios(
            Demand::contended_camera_above_the_floor(),
            Budget::Tight,
            ANY_FAULT,
        ),
        |scenario| {
            let report = scenario.run("no_inversion");
            prop_assume!(report.samples > 0 && report.received_bytes > 0);
            // Both must be running for their rungs to be comparable; a paused co-tenant is the
            // separate claim asserted above.
            let (Some(publisher), Some(cotenant)) = (
                report.forwarded_quality.get("publisher").copied(),
                report.forwarded_quality.get("cotenant").copied(),
            ) else {
                return Ok(());
            };
            prop_assume!(publisher > 0 && cotenant > 0);

            prop_assert!(
                publisher >= cotenant,
                "the publisher was asked for {}p and ended at layer {publisher}, below the \
                 co-tenant asked for 180p and served at layer {cotenant}, on a {} bps link \
                 ({scenario:?})",
                scenario.demand.target_height,
                scenario.capacity_bps,
            );
            Ok(())
        },
    );
}

/// The cast a churn sequence acts on. Small on purpose: the interesting states come from the
/// *order* things happen in, not from how many people are in the room.
const CHURN_CAST: [&str; 3] = ["churn1", "churn2", "churn3"];

/// One thing that can happen to a room.
///
/// Deliberately only lifecycle: joining, leaving cleanly, and crashing. Those are what move the
/// state the SFU keys on identity - routes, slots, subscriptions, assignments - and every state
/// bug found by hand has been a disagreement about who is in the room.
#[derive(Clone, Copy, Debug)]
enum Churn {
    Join(usize),
    Leave(usize),
    Crash(usize),
}

impl Churn {
    fn strategy() -> impl Strategy<Value = Churn> {
        let who = 0..CHURN_CAST.len();
        prop_oneof![
            who.clone().prop_map(Churn::Join),
            who.clone().prop_map(Churn::Leave),
            who.prop_map(Churn::Crash),
        ]
    }

    fn who(self) -> usize {
        match self {
            Churn::Join(i) | Churn::Leave(i) | Churn::Crash(i) => i,
        }
    }

    fn step(self) -> Step {
        match self {
            Churn::Join(i) => Step::Join {
                description: "somebody joins",
                participant: CHURN_CAST[i],
            },
            Churn::Leave(i) => Step::Disconnect {
                description: "somebody leaves",
                participant: CHURN_CAST[i],
            },
            Churn::Crash(i) => Step::AbruptExit {
                description: "somebody crashes",
                participant: CHURN_CAST[i],
            },
        }
    }
}

/// No sequence of arrivals, departures and crashes leaves anyone's view of the room wrong.
///
/// The authored plans cover the churn somebody thought of. This covers the churn nobody did: a
/// generated order of joins, clean leaves and crashes, run against the same room-state invariant
/// every plan is held to - nothing known that has left, nothing from a superseded incarnation, no
/// invented ids, and believed media kinds matching what is actually published.
///
/// Every state bug found by hand in this area was an ordering the suite happened not to contain: a
/// speaker's slot never released, a publisher's data route outliving them and colliding with their
/// own reconnect, a departed track reinstated by the next subscription. None needed an exotic
/// network - only the right sequence.
///
/// Run with buggify armed, because the interesting orderings are the ones a fault produces: a
/// route install that fails and is retried lands its effects in a different order than one that
/// succeeds, and that is precisely where identity-keyed state gets out of step.
#[test]
fn no_order_of_churn_leaves_a_wrong_view_of_the_room() {
    check(
        24,
        prop::collection::vec(Churn::strategy(), 2..7),
        |churn| {
            let mut room = Room::new("room1")
                .with_participant(Participant::single_publisher("anchor"))
                .with_participant(Participant::subscriber("viewer"));
            for who in CHURN_CAST {
                room =
                    room.with_participant(Participant::single_publisher(who).starts_disconnected());
            }

            let mut plan = vec![Step::Run {
                description: "Establish",
                duration: Duration::from_secs(3),
            }];
            // Only what could actually happen. Joining somebody already in the room, or removing
            // somebody who is not, is not churn - it is a nonsense instruction, and asserting on
            // the room afterwards asserts nothing. Walking the generated sequence and dropping
            // the impossible keeps shrinking useful while leaving every real ordering reachable.
            let mut present = [false; CHURN_CAST.len()];
            for op in &churn {
                let who = op.who();
                match op {
                    Churn::Join(_) if present[who] => continue,
                    Churn::Leave(_) | Churn::Crash(_) if !present[who] => continue,
                    _ => {}
                }
                present[who] = matches!(op, Churn::Join(_));
                plan.push(op.step());
                // The invariant is only asked of a settled room, and this is what settles it.
                plan.push(Step::Run {
                    description: "Settle after a change of membership",
                    duration: Duration::from_secs(6),
                });
            }

            LocalNodeSim::new().with_room(room).run(plan);
            Ok(())
        },
    );
}
