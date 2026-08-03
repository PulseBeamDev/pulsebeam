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
//! # Why the invariants look weak
//!
//! Deliberately. A property is claimed over every network the generator can produce, including
//! badly congested ones, so it can only assert what is true of *all* of them. "The estimate
//! reaches 80% of capacity" is false on a link too small to carry the layer, and a property that
//! is false for good reasons teaches nothing. What survives is the small set of things a
//! congestion controller must never do regardless of conditions - and those are exactly the ones
//! that were being violated in production.
//!
//! Case counts are low because each case is a full simulation. That is a real limit: this samples
//! the space rather than covering it. It is still strictly more than one hand-picked point, and
//! the seed makes anything it finds reproducible.

use super::common::{LinkReport, LocalNodeSim, Loss, Participant, Reorder, Room, Step};
use proptest::prelude::*;
use std::time::Duration;

/// A generated network and the load offered over it.
#[derive(Clone, Debug)]
struct Scenario {
    capacity_bps: u64,
    /// Whether the publisher is a camera (steady) or a screen share (bursty, long idle gaps).
    /// The idle case is what puts the sender in ALR, which is where the interesting failures are.
    screenshare: bool,
    /// Height the viewer asks for, which decides how far up the ladder demand goes.
    target_height: u32,
    settle_secs: u64,
    /// Whether a second publisher competes for the same viewer's link. Contention is a different
    /// axis from link quality: a controller can be perfectly well behaved about capacity and
    /// still let one stream starve to pay for another.
    contended: bool,
    /// Faults layered on top of the link. Generated together rather than one at a time: real
    /// paths present them at once, and the combinations are where a controller tuned against
    /// each in isolation comes apart.
    fault: Fault,
}

/// A path impairment applied for the life of the plan.
#[derive(Clone, Copy, Debug)]
enum Fault {
    None,
    /// Correlated loss, as wireless produces. Distinct from uniform loss at the same average.
    BurstLoss,
    /// Packets overtaken by their successors, which disturbs the delay signal and is separately
    /// counted as loss by the receiver until the gap fills.
    Reordering,
    /// A total interruption partway through, which the connection survives.
    Outage,
}

impl Scenario {
    /// Capacity generous enough to carry the top layer with room to spare.
    ///
    /// The ladder tops out at 1.25 Mbps, so anything from 2 Mbps up cannot be the reason a stream
    /// fails to be delivered. That is what makes a claim about these networks meaningful: there
    /// is no legitimate excuse available to the controller.
    fn healthy() -> impl Strategy<Value = Scenario> {
        (
            2_000_000u64..8_000_000,
            any::<bool>(),
            0usize..3,
            30u64..70,
            0usize..4,
            any::<bool>(),
        )
            .prop_map(
                |(capacity_bps, screenshare, height_idx, settle_secs, fault_idx, contended)| {
                    Scenario {
                        capacity_bps,
                        screenshare,
                        target_height: [180, 360, 720][height_idx],
                        settle_secs,
                        contended,
                        fault: [
                            Fault::None,
                            Fault::BurstLoss,
                            Fault::Reordering,
                            Fault::Outage,
                        ][fault_idx],
                    }
                },
            )
    }

    /// Steps that apply the generated fault, placed after the link has settled so the plan is
    /// measuring a controller meeting an impairment rather than starting inside one.
    ///
    /// The outage is bounded well under the point where ICE tears the session down. Past that it
    /// is not an impairment but a lost session, which is a different failure with a different fix
    /// and would leave these properties asserting whichever they happened to hit.
    fn fault_steps(&self) -> Vec<Step> {
        match self.fault {
            Fault::None => vec![],
            Fault::BurstLoss => vec![Step::SetLoss {
                description: "Wireless-style burst loss",
                participant: "viewer",
                loss: Loss::wifi(),
            }],
            Fault::Reordering => vec![Step::SetReorder {
                description: "Occasional out-of-order delivery",
                participant: "viewer",
                reorder: Reorder::occasional(),
            }],
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
        }
    }

    /// Like [`Scenario::healthy`] but always with a competing stream, for claims about
    /// contention specifically.
    fn contended() -> impl Strategy<Value = Scenario> {
        Self::healthy().prop_map(|s| Scenario {
            contended: true,
            ..s
        })
    }

    /// A subnet derived from the scenario, so identical scenarios get identical addresses.
    fn subnet(&self) -> u8 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.capacity_bps.hash(&mut hasher);
        self.screenshare.hash(&mut hasher);
        self.target_height.hash(&mut hasher);
        self.settle_secs.hash(&mut hasher);
        self.contended.hash(&mut hasher);
        format!("{:?}", self.fault).hash(&mut hasher);
        (hasher.finish() % 200) as u8
    }

    fn run(&self) -> LinkReport {
        let publisher = if self.screenshare {
            Participant::screensharer("publisher")
        } else {
            Participant::publisher("publisher", &["q", "h", "f"])
        };
        // The co-tenant is always asked for at its lowest rung. The claim being set up is that a
        // stream wanting very little is not starved outright, which is a far weaker thing to ask
        // than that both get what they want.
        let targets: &'static [(&'static str, u32)] = match (self.target_height, self.contended) {
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
                duration: Duration::from_secs(self.settle_secs),
            },
        ];
        plan.extend(self.fault_steps());
        // A fresh window last, so every claim describes the state the plan arrives at rather
        // than the disturbance on the way there.
        plan.push(Step::Run {
            description: "Measurement window",
            duration: Duration::from_secs(30),
        });

        let mut room = Room::new("room1").with_participant(publisher);
        if self.contended {
            room = room.with_participant(Participant::publisher("cotenant", &["q", "h", "f"]));
        }
        let slots = if self.contended { 2 } else { 1 };
        let reports = LocalNodeSim::new()
            // Addresses fixed by the scenario, so a replay of a recorded failure runs the same
            // network it originally did rather than whichever one its position happened to give.
            .with_subnet(self.subnet())
            .with_bandwidth(self.capacity_bps)
            .with_room(room.with_participant(Participant::multi_subscriber("viewer", slots)))
            .run_collecting(plan);
        reports
            .get("viewer")
            .cloned()
            .expect("the viewer should have been measured")
    }
}

fn config() -> ProptestConfig {
    ProptestConfig {
        // Each case is a full simulation of ~80s of network time. Low enough to keep a run
        // tolerable, high enough to reach networks nobody would have written down.
        cases: 12,
        max_shrink_iters: 8,
        failure_persistence: Some(Box::new(
            proptest::test_runner::FileFailurePersistence::WithSource("regressions"),
        )),
        ..ProptestConfig::default()
    }
}

proptest! {
    #![proptest_config(config())]

    /// On a link with room to spare, a subscribed stream must actually be delivered.
    ///
    /// The weakest possible statement of what an SFU is for, and the one production was
    /// violating: the screen share was subscribed, the link was fine, and nothing arrived. It
    /// holds for every network here by construction - capacity starts at 2 Mbps against a 1.25
    /// Mbps top layer - so a failure is never the link's fault.
    #[test]
    fn a_subscribed_stream_is_delivered_on_a_link_with_room(scenario in Scenario::healthy()) {
        let report = scenario.run();
        prop_assert!(
            report.samples > 0,
            "no allocation passes recorded; the plan never exercised the viewer, so every \
             other claim here would pass vacuously ({scenario:?})"
        );
        prop_assert!(
            report.received_bytes > 0,
            "nothing was delivered over 30s on a {} bps link carrying at most 1.25 Mbps of \
             video ({scenario:?}, report {report:?})",
            scenario.capacity_bps
        );
    }

    /// The estimate must not fall below what the application asked for, on a link that can
    /// supply it.
    ///
    /// This is the production failure stated as a property. The estimate is not required to find
    /// the capacity - it cannot, when the application is not using it - only to keep up with
    /// demand it is being asked to carry, on a link demonstrably able to carry it.
    #[test]
    fn the_estimate_keeps_up_with_demand_it_can_afford(scenario in Scenario::healthy()) {
        let report = scenario.run();
        prop_assume!(report.samples > 0 && report.received_bytes > 0);

        let need = report.need_bps();
        prop_assume!(need > 0);
        // Only meaningful where the link genuinely covers demand; otherwise falling short of
        // demand is correct behaviour rather than a defect.
        prop_assume!(scenario.capacity_bps > need + need / 4);

        let got = report.estimate_last_bps as f64 / need as f64 * 100.0;
        prop_assert!(
            got >= 60.0,
            "estimate ended at {} bps against a need of {need} bps ({got:.0}%) on a {} bps \
             link, having dropped {:.1}% from its peak with {:.2}% congestion loss ({scenario:?})",
            report.estimate_last_bps,
            scenario.capacity_bps,
            report.worst_drawdown_percent,
            report.congestion_loss_percent(),
        );
    }

    /// A controller must not sustain congestion loss on a link it is not filling.
    ///
    /// Overuse is defined by the bottleneck's own tail-drop, not by a bitrate anyone chose, so
    /// this holds for any capacity: if the link is not being filled, packets have no business
    /// being dropped for congestion.
    #[test]
    fn an_underused_link_is_not_driven_into_loss(scenario in Scenario::healthy()) {
        let report = scenario.run();
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
    }
}

proptest! {
    #![proptest_config(config())]

    /// A stream asking for the least the ladder offers must not be starved to pay for another.
    ///
    /// The production report was "the allocator doesn't think there is enough for the camera",
    /// with the camera paused while a screen share ran. The generator makes the co-tenant ask for
    /// the bottom rung only - 150 kbps against links from 2 Mbps up - so there is no capacity
    /// argument available: a stream left paused here was not priced out, it was overlooked.
    ///
    /// Stated as "not paused" rather than "gets its fair share". What fairness means between a
    /// screen share and a camera is a policy question with defensible answers either way; being
    /// dropped entirely is not one of them.
    #[test]
    fn a_cheap_co_tenant_is_not_starved(scenario in Scenario::contended()) {
        let report = scenario.run();
        prop_assume!(report.samples > 0 && report.received_bytes > 0);

        // Conditioned on the *estimate* covering demand, not the link. The allocator can only
        // spend what it is given, so a starved co-tenant on a healthy link with a low estimate is
        // a bandwidth-estimation failure and belongs to the property above - conflating the two
        // here would leave neither diagnosable. What remains is the allocator's own claim: when
        // there is demonstrably enough to go round, nobody is dropped.
        //
        // Under genuine contention someone must be paused, and a single-layer screen share has no
        // tier to shed, so pausing the cheaper stream is defensible rather than a defect.
        prop_assume!(report.estimate_last_bps > report.demand_last_bps);

        let quality = report.forwarded_quality.get("cotenant").copied();
        prop_assert!(
            quality.is_some_and(|q| q > 0),
            "the co-tenant was left at {quality:?} (0 = paused) on a {} bps link that covers the \
             full {} bps of demand, so nothing was priced out. The other stream ended at {:?}. \
             ({scenario:?}, estimate {} bps, {:.2}% congestion loss)",
            scenario.capacity_bps,
            report.demand_last_bps,
            report.forwarded_quality.get("publisher"),
            report.estimate_last_bps,
            report.congestion_loss_percent(),
        );
    }
}
