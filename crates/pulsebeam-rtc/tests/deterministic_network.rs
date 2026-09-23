#![allow(
    clippy::arithmetic_side_effects,
    clippy::disallowed_types,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr,
    reason = "the deterministic system harness prints its seed/trace and fails at the first invariant"
)]

mod support;

use std::time::Duration;

use bytes::Bytes;
use pulsebeam_rtc::{
    CloseReason, Command, CommandError, ConnectionConfig, MediaPayloadBitrate, NetworkInput,
    Output, PlayoutDelay,
};
use support::{NetworkPolicy, PeerFixture, forwarded};

const PAYLOAD_BYTES: usize = 1_000;
const FEEDBACK_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug)]
struct Scenario {
    name: &'static str,
    seed: u64,
    duration: Duration,
    policy: NetworkPolicy,
    packets: u64,
    transport: Transport,
}

#[derive(Clone, Copy, Debug)]
enum Transport {
    Udp,
    Tcp,
    Data,
}

const SCENARIOS: [Scenario; 11] = [
    scenario("startup", 0x1101, 10, 40),
    Scenario {
        name: "vbr-keyframe",
        seed: 0x1102,
        duration: Duration::from_secs(15),
        policy: NetworkPolicy {
            delay: Duration::from_millis(40),
            drop_every: Some(100),
            duplicate_every: None,
            reorder_every: Some(50),
        },
        packets: 512,
        transport: Transport::Udp,
    },
    scenario("bandwidth-rtt-steps", 0x1103, 15, 96),
    Scenario {
        name: "feedback-outage",
        seed: 0x1104,
        duration: Duration::from_secs(10),
        policy: NetworkPolicy {
            delay: Duration::from_millis(50),
            drop_every: Some(3),
            duplicate_every: None,
            reorder_every: None,
        },
        packets: 80,
        transport: Transport::Udp,
    },
    Scenario {
        name: "ecn",
        seed: 0x1105,
        duration: Duration::from_secs(10),
        policy: NetworkPolicy {
            delay: Duration::from_millis(25),
            drop_every: None,
            duplicate_every: Some(20),
            reorder_every: None,
        },
        packets: 80,
        transport: Transport::Udp,
    },
    scenario("policer-competition", 0x1106, 20, 160),
    scenario("pause-switch", 0x1107, 12, 48),
    Scenario {
        name: "path-replacement",
        seed: 0x1108,
        duration: Duration::from_secs(12),
        policy: NetworkPolicy {
            delay: Duration::from_millis(25),
            drop_every: Some(2),
            duplicate_every: None,
            reorder_every: Some(1),
        },
        packets: 48,
        transport: Transport::Tcp,
    },
    Scenario {
        name: "sctp-coexistence",
        seed: 0x1109,
        duration: Duration::from_secs(10),
        policy: NetworkPolicy {
            delay: Duration::from_millis(40),
            drop_every: None,
            duplicate_every: None,
            reorder_every: None,
        },
        packets: 64,
        transport: Transport::Data,
    },
    Scenario {
        name: "overload-malformed",
        seed: 0x1110,
        duration: Duration::from_secs(1),
        policy: NetworkPolicy {
            delay: Duration::ZERO,
            drop_every: None,
            duplicate_every: None,
            reorder_every: Some(7),
        },
        packets: 64,
        transport: Transport::Udp,
    },
    scenario("close", 0x1111, 2, 32),
];

const fn scenario(name: &'static str, seed: u64, seconds: u64, packets: u64) -> Scenario {
    Scenario {
        name,
        seed,
        duration: Duration::from_secs(seconds),
        policy: NetworkPolicy {
            delay: Duration::from_millis(25),
            drop_every: None,
            duplicate_every: None,
            reorder_every: None,
        },
        packets,
        transport: Transport::Udp,
    }
}

fn assert_probe_windows(emitted: &[(std::time::Instant, u64, bool)]) {
    let mut checkpoints = Vec::with_capacity(emitted.len() * 2);
    for (at, _, _) in emitted {
        checkpoints.push(*at);
        checkpoints.push(*at + Duration::from_secs(5));
    }
    checkpoints.sort_unstable();
    checkpoints.dedup();
    let mut peak_ratio = 0_u64;
    let mut peak_allowance = 0_u64;
    for at in checkpoints {
        let mut nonprobe = 0_u64;
        let mut probes = 0_u64;
        for (sent, bytes, probe) in emitted {
            if *sent <= at && at.saturating_duration_since(*sent) < Duration::from_secs(5) {
                if *probe {
                    probes = probes.saturating_add(*bytes);
                } else {
                    nonprobe = nonprobe.saturating_add(*bytes);
                }
            }
        }
        let total = probes.saturating_add(nonprobe);
        peak_ratio = peak_ratio.max(if total == 0 { 0 } else { probes * 100 / total });
        peak_allowance = peak_allowance.max(probes.saturating_sub(nonprobe / 19));
        assert!(
            probes <= nonprobe / 19 + 3_000,
            "emitted probe budget at {at:?}: probes={probes} nonprobe={nonprobe}"
        );
    }
    eprintln!(
        "production probe windows events={} peak_raw_ratio={peak_ratio}% peak_allowance_used={peak_allowance}",
        emitted.len()
    );
}

#[test]
fn production_pre_media_probes_use_only_the_low_traffic_allowance() {
    assert_probe_windows(&[]);
    let mut fixture = PeerFixture::connected();
    fixture.drive_for(Duration::from_secs(6));
    assert_probe_windows(fixture.emitted_rtp());
    let totals = fixture.connection.stats().connection;
    assert_eq!(totals.transmitted_rtp_bytes, 0);
    assert!(totals.transmitted_padding_bytes > 0);
    assert!(totals.transmitted_padding_bytes <= 3_000);
    eprintln!(
        "probe-only window nonprobe=0 padding={} raw_ratio=100% allowance_used={}",
        totals.transmitted_padding_bytes, totals.transmitted_padding_bytes
    );
}

#[test]
fn production_connection_bottleneck_emits_measured_transport_bytes() {
    const SEED: u64 = 0x1122;
    const RATE_BPS: u64 = 100_000;
    const PACKETS: u64 = 1_500;
    let mut fixture = PeerFixture::connected();
    fixture.configure_network(
        SEED,
        NetworkPolicy {
            delay: Duration::from_millis(25),
            ..NetworkPolicy::default()
        },
    );
    fixture.configure_bottleneck(RATE_BPS);
    let started = fixture.at().monotonic;
    let mut source = PeerFixture::connected();
    let mut emitted_windows =
        std::collections::VecDeque::from([(fixture.at().monotonic, 0_u64, 0_u64)]);
    let mut max_probe_percent = 0_u64;
    let mut measured_windows = 0_u64;
    for id in 1..=PACKETS {
        source.drive_for(
            fixture
                .at()
                .monotonic
                .saturating_duration_since(source.at().monotonic)
                + Duration::from_millis(40),
        );
        let media = forwarded(source.send_source(&[0x5a; PAYLOAD_BYTES]), id);
        match fixture.connection.command(
            fixture.at(),
            Command::SendMedia {
                sender: fixture.sender,
                media,
            },
        ) {
            Ok(()) | Err(CommandError::WouldBlock) => {}
            Err(error) => panic!("production admission at {id}: {error:?}"),
        }
        fixture.drive_for(Duration::from_millis(40));
        let totals = fixture.connection.stats().connection;
        emitted_windows.push_back((
            fixture.at().monotonic,
            totals.transmitted_rtp_bytes,
            totals.transmitted_padding_bytes,
        ));
        while emitted_windows.get(1).is_some_and(|(at, _, _)| {
            fixture.at().monotonic.saturating_duration_since(*at) >= Duration::from_secs(5)
        }) {
            emitted_windows.pop_front();
        }
        if let Some((start, rtp, padding)) = emitted_windows.front()
            && fixture.at().monotonic.saturating_duration_since(*start) >= Duration::from_secs(5)
        {
            measured_windows += 1;
            let emitted = totals.transmitted_rtp_bytes.saturating_sub(*rtp);
            let probes = totals.transmitted_padding_bytes.saturating_sub(*padding);
            let total = emitted.saturating_add(probes);
            max_probe_percent = max_probe_percent.max(if total == 0 {
                0
            } else {
                probes.saturating_mul(100) / total
            });
            assert!(probes <= emitted / 19 + 3_000);
        }
    }
    fixture.drive_for(Duration::from_secs(2));
    assert_probe_windows(fixture.emitted_rtp());
    let (sojourn, delivered) = fixture.bottleneck_samples();
    let mut ordered = sojourn.to_vec();
    ordered.sort_unstable();
    let p99 = ordered[ordered.len() * 99 / 100];
    let stats = fixture.connection.stats();
    eprintln!(
        "production bottleneck seed={SEED:#06x} rate={RATE_BPS} samples={} delivered_bytes={delivered} emitted_rtp_bytes={} padding_bytes={} target_bps={} max_rolling_probe_percent={max_probe_percent} measured_windows={measured_windows} queue_p99_ms={}",
        sojourn.len(),
        stats.connection.transmitted_rtp_bytes,
        stats.connection.transmitted_padding_bytes,
        stats.connection.target_media_bitrate.as_bps(),
        p99.as_millis()
    );
    assert!(
        sojourn.len() >= 1_000,
        "insufficient emitted transport samples"
    );
    let elapsed_micros = fixture.at().monotonic.duration_since(started).as_micros();
    let service_bytes = u128::from(RATE_BPS).saturating_mul(elapsed_micros) / 8_000_000;
    assert!(
        u128::from(delivered).saturating_mul(100) >= service_bytes.saturating_mul(85),
        "non-outage bottleneck utilization must be at least 85%; delivered={delivered} service={service_bytes} elapsed_us={elapsed_micros}"
    );
    assert!(stats.connection.transmitted_rtp_bytes > 0);
    assert!(stats.connection.transmitted_padding_bytes <= stats.connection.transmitted_rtp_bytes);
    assert!(measured_windows >= 1_000);
}

#[test]
fn production_two_megabit_single_flow_capacity() {
    let mut fixture = PeerFixture::connected();
    fixture.configure_network(
        0x2201,
        NetworkPolicy {
            delay: Duration::from_millis(25),
            ..NetworkPolicy::default()
        },
    );
    fixture.configure_bottleneck(2_000_000);
    fixture.configure_time_quantum(Duration::from_millis(2));
    let mut policy = ConnectionConfig::default().default_audio_policy;
    policy.desired_bitrate = MediaPayloadBitrate::from_bps(4_000_000);
    policy.playout_delay = PlayoutDelay::from_ticks(0, 50).expect("500ms playout");
    fixture
        .connection
        .command(
            fixture.at(),
            Command::SetSenderPolicy {
                sender: fixture.sender,
                policy,
            },
        )
        .expect("demand policy");
    let start = fixture.at().monotonic;
    let mut source = PeerFixture::connected();
    source.configure_time_quantum(Duration::from_millis(2));
    for id in 1..=6_000_u64 {
        source.drive_for(
            fixture
                .at()
                .monotonic
                .saturating_duration_since(source.at().monotonic)
                + Duration::from_millis(2),
        );
        let media = forwarded(source.send_source(&[0x5a; PAYLOAD_BYTES]), id);
        match fixture.connection.command(
            fixture.at(),
            Command::SendMedia {
                sender: fixture.sender,
                media,
            },
        ) {
            Ok(()) | Err(CommandError::WouldBlock) => {}
            Err(error) => panic!("admission: {error:?}"),
        }
        fixture.drive_for(Duration::from_millis(2));
    }
    let end = fixture.at().monotonic;
    let stable_start = start + Duration::from_secs(2);
    let cohort = fixture
        .emitted_rtp()
        .iter()
        .filter(|(at, _, _)| *at >= stable_start && *at < end)
        .map(|(_, bytes, _)| *bytes)
        .sum::<u64>();
    fixture.drive_for(Duration::from_secs(2));
    assert_probe_windows(fixture.emitted_rtp());
    let delivered = fixture
        .delivered_rtp()
        .iter()
        .filter(|(at, _)| *at >= stable_start && *at < end)
        .map(|(_, bytes)| *bytes)
        .sum::<u64>();
    let service = 2_000_000_u128 * end.duration_since(stable_start).as_micros() / 8_000_000;
    eprintln!(
        "2mbps seed=0x2201 stable_emitted={cohort} stable_delivered={delivered} service={service} duration={:?}",
        end.duration_since(stable_start)
    );
    assert!(end.duration_since(stable_start) >= Duration::from_secs(10));
    assert_eq!(delivered, cohort, "all emission-cohort RTP must drain");
    assert!(fixture.bottleneck_samples().0.len() >= 1_000);
    assert!(u128::from(delivered) * 100 >= service * 85);
}

#[test]
fn production_capacity_steps_recover_stable_transport_service() {
    let mut fixture = PeerFixture::connected();
    fixture.configure_network(
        0x6601,
        NetworkPolicy {
            delay: Duration::from_millis(25),
            ..NetworkPolicy::default()
        },
    );
    fixture.configure_bottleneck(2_000_000);
    fixture.configure_time_quantum(Duration::from_millis(2));
    let mut policy = ConnectionConfig::default().default_audio_policy;
    policy.desired_bitrate = MediaPayloadBitrate::from_bps(4_000_000);
    policy.playout_delay = PlayoutDelay::from_ticks(0, 50).expect("500ms playout");
    fixture.command(Command::SetSenderPolicy {
        sender: fixture.sender,
        policy,
    });
    let mut source = PeerFixture::connected();
    source.configure_time_quantum(Duration::from_millis(2));
    let start = fixture.at().monotonic;
    for id in 1..=18_000_u64 {
        let tick = start + Duration::from_millis(id * 2);
        fixture.drive_for(tick.saturating_duration_since(fixture.at().monotonic));
        if id == 6_000 {
            fixture.set_bottleneck_rate(1_000_000);
        }
        if id == 12_000 {
            fixture.set_bottleneck_rate(2_000_000);
        }
        source.drive_for(tick.saturating_duration_since(source.at().monotonic));
        let media = forwarded(source.send_source(&[0x5a; PAYLOAD_BYTES]), id);
        match fixture.try_command(Command::SendMedia {
            sender: fixture.sender,
            media,
        }) {
            Ok(()) | Err(CommandError::WouldBlock) => {}
            Err(error) => panic!("step admission: {error:?}"),
        }
    }
    fixture.drive_for(Duration::from_secs(2));
    for (from, to, rate) in [
        (2, 12, 2_000_000_u64),
        (14, 24, 1_000_000),
        (26, 36, 2_000_000),
    ] {
        let begin = start + Duration::from_secs(from);
        let end = start + Duration::from_secs(to);
        let emitted = fixture
            .emitted_rtp()
            .iter()
            .filter(|(at, _, _)| *at >= begin && *at < end)
            .map(|(_, bytes, _)| *bytes)
            .sum::<u64>();
        let delivered = fixture
            .delivered_rtp()
            .iter()
            .filter(|(at, _)| *at >= begin && *at < end)
            .map(|(_, bytes)| *bytes)
            .sum::<u64>();
        let service = rate / 8 * 10;
        eprintln!(
            "capacity-step seed=0x6601 interval={from}..{to} rate={rate} emitted={emitted} delivered={delivered} service={service}"
        );
        assert_eq!(delivered, emitted, "stable emission cohort must drain");
        assert!(u128::from(delivered) * 100 >= u128::from(service) * 85);
    }
}

#[test]
fn production_homogeneous_shared_bottleneck_competition() {
    let shared = std::rc::Rc::new(std::cell::RefCell::new(None));
    let mut flows = vec![PeerFixture::connected(), PeerFixture::connected()];
    let mut sources = vec![PeerFixture::connected(), PeerFixture::connected()];
    for (index, flow) in flows.iter_mut().enumerate() {
        flow.configure_network(
            0x3301 + index as u64,
            NetworkPolicy {
                delay: Duration::from_millis(25),
                ..NetworkPolicy::default()
            },
        );
        flow.configure_bottleneck(4_000_000);
        flow.share_bottleneck_departure(shared.clone());
        flow.configure_time_quantum(Duration::from_millis(2));
        let mut policy = ConnectionConfig::default().default_audio_policy;
        policy.desired_bitrate = MediaPayloadBitrate::from_bps(4_000_000);
        policy.playout_delay = PlayoutDelay::from_ticks(0, 50).expect("500ms playout");
        flow.connection
            .command(
                flow.at(),
                Command::SetSenderPolicy {
                    sender: flow.sender,
                    policy,
                },
            )
            .expect("demand policy");
    }
    for source in &mut sources {
        source.configure_time_quantum(Duration::from_millis(2));
    }
    let start = flows.iter().map(|flow| flow.at().monotonic).max().unwrap();
    for id in 1..=6_000_u64 {
        let tick = start + Duration::from_millis(id * 2);
        for (flow, source) in flows.iter_mut().zip(sources.iter_mut()) {
            flow.drive_for(tick.saturating_duration_since(flow.at().monotonic));
            source.drive_for(tick.saturating_duration_since(source.at().monotonic));
            let media = forwarded(source.send_source(&[0x5a; PAYLOAD_BYTES]), id);
            match flow.connection.command(
                flow.at(),
                Command::SendMedia {
                    sender: flow.sender,
                    media,
                },
            ) {
                Ok(()) | Err(CommandError::WouldBlock) => {}
                Err(error) => panic!("shared-flow admission: {error:?}"),
            }
        }
    }
    for flow in &mut flows {
        flow.drive_for(
            (start + Duration::from_secs(12)).saturating_duration_since(flow.at().monotonic),
        );
    }
    let end = flows.iter().map(|flow| flow.at().monotonic).min().unwrap();
    let stable_start = start + Duration::from_secs(2);
    for flow in &mut flows {
        flow.drive_for(Duration::from_secs(2));
    }
    let emitted: [u64; 2] = std::array::from_fn(|index| {
        assert_probe_windows(flows[index].emitted_rtp());
        flows[index]
            .emitted_rtp()
            .iter()
            .filter(|(at, _, _)| *at >= stable_start && *at < end)
            .map(|(_, bytes, _)| *bytes)
            .sum()
    });
    let delivered: [u64; 2] = std::array::from_fn(|index| {
        flows[index]
            .delivered_rtp()
            .iter()
            .filter(|(at, _)| *at >= stable_start && *at < end)
            .map(|(_, bytes)| *bytes)
            .sum::<u64>()
    });
    let combined = delivered.iter().sum::<u64>();
    let service = 4_000_000_u128 * end.duration_since(stable_start).as_micros() / 8_000_000;
    eprintln!(
        "competition seed=0x3301/0x3302 delivered={delivered:?} service={service} duration={:?} end_skew={:?} targets={:?} emitted={:?}",
        end.duration_since(stable_start),
        flows[0]
            .at()
            .monotonic
            .max(flows[1].at().monotonic)
            .duration_since(flows[0].at().monotonic.min(flows[1].at().monotonic)),
        flows
            .iter()
            .map(|flow| flow
                .connection
                .stats()
                .connection
                .target_media_bitrate
                .as_bps())
            .collect::<Vec<_>>(),
        flows
            .iter()
            .map(|flow| flow.connection.stats().connection.transmitted_rtp_bytes)
            .collect::<Vec<_>>()
    );
    assert!(end.duration_since(stable_start) >= Duration::from_secs(10));
    assert_eq!(delivered, emitted, "both emission cohorts must drain");
    assert!(u128::from(combined) * 100 >= service * 85);
    for bytes in delivered {
        assert!(u128::from(bytes) * 100 >= u128::from(combined) * 40);
        assert!(u128::from(bytes) * 100 <= u128::from(combined) * 60);
    }
}

#[test]
fn production_two_sender_allocation_matches_payload_service() {
    let mut fixture = PeerFixture::connected_with_senders(2);
    fixture.configure_network(
        0x4401,
        NetworkPolicy {
            delay: Duration::from_millis(25),
            ..NetworkPolicy::default()
        },
    );
    fixture.configure_bottleneck(2_000_000);
    fixture.configure_time_quantum(Duration::from_millis(2));
    for sender in &fixture.senders {
        let mut policy = ConnectionConfig::default().default_audio_policy;
        policy.desired_bitrate = MediaPayloadBitrate::from_bps(4_000_000);
        policy.playout_delay = PlayoutDelay::from_ticks(0, 50).expect("500ms playout");
        fixture
            .connection
            .command(
                fixture.at(),
                Command::SetSenderPolicy {
                    sender: *sender,
                    policy,
                },
            )
            .expect("sender demand");
    }
    let mut sources = vec![PeerFixture::connected(), PeerFixture::connected()];
    for source in &mut sources {
        source.configure_time_quantum(Duration::from_millis(2));
    }
    let start = fixture.at().monotonic;
    let mut previous_at = start + Duration::from_secs(2);
    let mut integrated = [0_u128; 2];
    let mut payload_start = [0_u64; 2];
    let mut admitted = [0_u64; 2];
    let mut blocked = [0_u64; 2];
    for id in 1..=6_000_u64 {
        let tick = start + Duration::from_millis(id * 2);
        fixture.drive_for(tick.saturating_duration_since(fixture.at().monotonic));
        let first = usize::try_from(id % 2).unwrap_or_default();
        for index in [first, 1 - first] {
            let source = &mut sources[index];
            source.drive_for(tick.saturating_duration_since(source.at().monotonic));
            let media = forwarded(source.send_source(&[0x5a; PAYLOAD_BYTES]), id);
            match fixture.try_command(Command::SendMedia {
                sender: fixture.senders[index],
                media,
            }) {
                Ok(()) => admitted[index] += 1,
                Err(CommandError::WouldBlock) => blocked[index] += 1,
                Err(error) => panic!("two-sender admission: {error:?}"),
            }
        }
        let now = fixture.at().monotonic;
        let stats = fixture.connection.stats();
        if now < previous_at {
            for (index, sender) in stats.senders.iter().enumerate() {
                payload_start[index] = sender.transmitted_payload_bytes;
            }
        } else {
            let dt = now.saturating_duration_since(previous_at).as_micros();
            for (index, sender) in stats.senders.iter().enumerate() {
                integrated[index] += u128::from(sender.allocation.as_bps()) * dt;
            }
            previous_at = now;
        }
    }
    assert!(
        fixture
            .at()
            .monotonic
            .duration_since(start + Duration::from_secs(2))
            >= Duration::from_secs(10)
    );
    fixture.drive_for(Duration::from_secs(2));
    let stats = fixture.connection.stats();
    assert_eq!(
        fixture.network_counters().1,
        0,
        "loss-free allocation fixture"
    );
    assert_eq!(
        fixture
            .delivered_rtp()
            .iter()
            .map(|(_, bytes)| *bytes)
            .sum::<u64>(),
        stats
            .connection
            .transmitted_rtp_bytes
            .saturating_add(stats.connection.transmitted_padding_bytes),
        "all emitted RTP including probes delivered"
    );
    let delivered = [0, 1].map(|i| stats.senders[i].transmitted_payload_bytes - payload_start[i]);
    eprintln!(
        "two-sender seed=0x4401 admitted={admitted:?} blocked={blocked:?} integrated_allocation={integrated:?} payload={delivered:?} end_stats={:?}",
        stats
            .senders
            .iter()
            .map(|sender| (
                sender.transmitted_packets,
                sender.queued_packets,
                sender.allocation.as_bps()
            ))
            .collect::<Vec<_>>()
    );
    let total = integrated.iter().sum::<u128>();
    let total_payload = delivered.iter().sum::<u64>();
    assert!(total > 0 && total_payload > 0);
    for value in integrated {
        assert!(value * 100 >= total * 45 && value * 100 <= total * 55);
    }
    for value in delivered {
        assert!(u128::from(value) * 100 >= u128::from(total_payload) * 45);
        assert!(u128::from(value) * 100 <= u128::from(total_payload) * 55);
    }
}

#[test]
fn fixed_system_matrix() {
    assert_eq!(PAYLOAD_BYTES, 1_000);
    assert_eq!(FEEDBACK_INTERVAL, Duration::from_millis(50));
    let checked_in_trace = include_str!("traces/system-v1.txt");
    assert_eq!(checked_in_trace.lines().count(), SCENARIOS.len());

    for scenario in SCENARIOS {
        assert!(checked_in_trace.contains(scenario.name));
        assert!(checked_in_trace.contains(&format!("{:#06x}", scenario.seed)));
        run(scenario);
    }
}

fn run(scenario: Scenario) {
    let mut fixture = match (scenario.name, scenario.transport) {
        ("policer-competition", _) => PeerFixture::connected_with_senders(2),
        (_, Transport::Udp) => PeerFixture::connected(),
        (_, Transport::Tcp) => PeerFixture::connected_tcp(),
        (_, Transport::Data) => PeerFixture::connected_datachannels(),
    };
    if matches!(scenario.transport, Transport::Data) {
        while !matches!(
            fixture.next_data_event(),
            support::FixtureDataEvent::ConnectionOpened(_)
        ) {}
    }
    let mut source = PeerFixture::connected();
    let mut alternate_source = PeerFixture::connected();
    fixture.configure_network(scenario.seed, scenario.policy);

    let mut admitted = 0_u64;
    for id in 1..=scenario.packets {
        source.drive_for(Duration::from_millis(20));
        alternate_source.drive_for(Duration::from_millis(20));
        let selected_source = if scenario.name == "pause-switch" && id > scenario.packets / 2 {
            &mut alternate_source
        } else {
            &mut source
        };
        let packet = selected_source.send_source(&vec![0x5a; PAYLOAD_BYTES]);
        let media = forwarded(packet, id);
        let sender = if scenario.name == "policer-competition" {
            *fixture
                .senders
                .get(usize::try_from(id % 2).expect("flow index"))
                .expect("two competing flows")
        } else {
            fixture.sender
        };
        for _ in 0..20 {
            match fixture.connection.command(
                fixture.at(),
                Command::SendMedia {
                    sender,
                    media: media.clone(),
                },
            ) {
                Ok(()) => {
                    admitted = admitted.saturating_add(1);
                    break;
                }
                Err(CommandError::WouldBlock) => fixture.drive_for(FEEDBACK_INTERVAL),
                Err(error) => panic!("scenario admission failed: {error:?}"),
            }
        }
    }

    if matches!(scenario.transport, Transport::Data) {
        fixture.peer_send(true, &vec![0xa5; 64 * 1024]);
    }

    if scenario.name == "overload-malformed" {
        fixture
            .connection
            .receive(
                fixture.at(),
                NetworkInput::Udp {
                    local: "127.0.0.1:41000".parse().expect("local address"),
                    remote: "127.0.0.1:9".parse().expect("wrong-path address"),
                    ecn: None,
                    payload: Bytes::from_static(b"malformed"),
                },
            )
            .expect("malformed traffic is isolated");
    }

    fixture.drive_for(scenario.duration);
    let stats = fixture.connection.stats();
    let sender = stats.senders.first().expect("negotiated sender stats");
    let transmitted = stats
        .senders
        .iter()
        .map(|sender| sender.transmitted_packets)
        .sum::<u64>();
    let (network_packets, dropped, duplicated, reordered) = fixture.network_counters();

    assert!(transmitted <= admitted);
    assert!(sender.queued_packets <= 8_192);
    assert!(stats.connection.queued_media_bytes <= 8 * 1024 * 1024);
    assert!(stats.connection.rtp_bytes_in_flight <= 8 * 1024 * 1024);
    assert!(stats.connection.duplicate_feedback <= stats.connection.transmitted_rtp_bytes);
    assert!(
        stats.connection.transmitted_sctp_bytes == 0
            || matches!(scenario.transport, Transport::Data)
    );
    if matches!(scenario.transport, Transport::Data) {
        assert!(stats.connection.transmitted_sctp_bytes > 0);
    }
    assert!(network_packets >= transmitted);
    if scenario.policy.drop_every.is_some() {
        assert!(dropped > 0, "configured loss must occur");
    }
    if scenario.policy.duplicate_every.is_some() {
        assert!(duplicated > 0, "configured duplication must occur");
    }
    if scenario.policy.reorder_every.is_some() {
        assert!(reordered > 0, "configured reordering must occur");
    }
    if scenario.name == "startup" {
        assert!(transmitted * 100 >= admitted * 85);
    }
    if scenario.name == "policer-competition" {
        let delivered = stats
            .senders
            .iter()
            .map(|sender| sender.transmitted_payload_bytes)
            .sum::<u64>();
        let first_share = stats
            .senders
            .first()
            .expect("first competing flow")
            .transmitted_payload_bytes
            * 100
            / delivered.max(1);
        assert!((40..=60).contains(&first_share));
    }
    eprintln!(
        "scenario={} seed={:#06x} duration={:?} packets={} admitted={} transmitted={} network={} dropped={} duplicated={} reordered={} queue_bytes={} bif={} trace={:?}",
        scenario.name,
        scenario.seed,
        scenario.duration,
        scenario.packets,
        admitted,
        transmitted,
        network_packets,
        dropped,
        duplicated,
        reordered,
        stats.connection.queued_media_bytes,
        stats.connection.rtp_bytes_in_flight,
        fixture.network_trace(),
    );

    fixture.command(Command::Abort);
    assert!(matches!(
        fixture.connection.poll(fixture.at()),
        Output::Closed(CloseReason::Aborted)
    ));
    assert!(matches!(
        fixture.connection.poll(fixture.at()),
        Output::Idle { next_wakeup: None }
    ));
}
