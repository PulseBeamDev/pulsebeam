#![allow(
    clippy::arithmetic_side_effects,
    clippy::disallowed_types,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr,
    reason = "the deterministic system harness prints its seed/trace and fails at the first invariant"
)]

use std::time::Duration;

use crate::test_source::ComponentSource;
use crate::test_support::{NetworkPolicy, PeerFixture, assert_probe_windows, forwarded};
use crate::{Command, CommandError, ConnectionConfig, MediaPayloadBitrate, PlayoutDelay};

const PAYLOAD_BYTES: usize = 1_000;

#[test]
fn production_connection_bottleneck_emits_measured_transport_bytes() {
    // Keep packet serialization below the unchanged 60 ms queue-delay target.
    const PAYLOAD_BYTES: usize = 500;
    const SEED: u64 = 0x1122;
    const RATE_BPS: u64 = 100_000;
    const PACKETS: u64 = 4_000;
    let origin = std::time::Instant::now();
    let mut fixture = PeerFixture::connected_at(origin);
    fixture.configure_network(
        SEED,
        NetworkPolicy {
            delay: Duration::from_millis(25),
            ..NetworkPolicy::default()
        },
    );
    fixture.configure_bottleneck(RATE_BPS);
    fixture.configure_time_quantum(Duration::from_millis(5));
    let mut source = ComponentSource::audio(1);
    let mut policy = pulsebeam_rtc::ConnectionConfig::default().default_audio_policy;
    policy.desired_bitrate = MediaPayloadBitrate::from_bps(1_000_000);
    // At 100 kbit/s, a full minimum send window alone takes hundreds of ms.
    // This is a transport-service fixture, not a low-latency policy fixture.
    policy.playout_delay = pulsebeam_rtc::PlayoutDelay::from_ticks(0, 200).expect("2 seconds");
    fixture.command(Command::SetSenderPolicy {
        sender: fixture.sender,
        policy,
    });
    let started = fixture.at().monotonic;
    fixture.drive_for(started.saturating_duration_since(fixture.at().monotonic));
    let mut emitted_windows =
        std::collections::VecDeque::from([(fixture.at().monotonic, 0_u64, 0_u64)]);
    let mut max_probe_percent = 0_u64;
    let mut measured_windows = 0_u64;
    for id in 1..=PACKETS {
        let at = started + Duration::from_millis(id * 40);
        fixture.drive_for(at.saturating_duration_since(fixture.at().monotonic));
        let media = forwarded(source.sample(fixture.at(), &[0x5a; PAYLOAD_BYTES]), id);
        match fixture.try_command(Command::SendMedia {
            sender: fixture.sender,
            media,
        }) {
            Ok(()) | Err(CommandError::WouldBlock) => {}
            Err(error) => panic!("production admission at {id}: {error:?}"),
        }
        fixture.drive_for(Duration::from_millis(1));
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
    let mut source = ComponentSource::audio(1);
    for id in 1..=6_000_u64 {
        let media = forwarded(source.sample(fixture.at(), &[0x5a; PAYLOAD_BYTES]), id);
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
    let mut source = ComponentSource::audio(1);
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
        let media = forwarded(source.sample(fixture.at(), &[0x5a; PAYLOAD_BYTES]), id);
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
    let mut sources = [ComponentSource::audio(1), ComponentSource::audio(2)];
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
    let start = flows.iter().map(|flow| flow.at().monotonic).max().unwrap();
    for id in 1..=6_000_u64 {
        let tick = start + Duration::from_millis(id * 2);
        for (flow, source) in flows.iter_mut().zip(sources.iter_mut()) {
            flow.drive_for(tick.saturating_duration_since(flow.at().monotonic));
            let media = forwarded(source.sample(flow.at(), &[0x5a; PAYLOAD_BYTES]), id);
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
    for (weights, desired) in [
        ([1, 1], [4_000_000, 4_000_000]),
        ([1, 2], [4_000_000, 4_000_000]),
        ([1, 2], [200_000, 4_000_000]),
        ([1, 2], [0, 4_000_000]),
    ] {
        check_two_sender_allocation(weights, desired);
    }
}

#[allow(
    clippy::indexing_slicing,
    reason = "the two-sender fixture uses matching two-element measurement arrays"
)]
fn check_two_sender_allocation(weights: [u16; 2], desired: [u64; 2]) {
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
    for (index, sender) in fixture.senders.iter().enumerate() {
        let mut policy = ConnectionConfig::default().default_audio_policy;
        policy.priority = pulsebeam_rtc::MediaPriority::new(weights[index]).expect("priority");
        policy.desired_bitrate = MediaPayloadBitrate::from_bps(desired[index]);
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
    let mut sources = [ComponentSource::audio(1), ComponentSource::audio(2)];
    let start = fixture.at().monotonic;
    let mut previous_at = start + Duration::from_secs(2);
    let mut integrated = [0_u128; 2];
    let mut expected = [0_u128; 2];
    // The fixed 500 ms policy uses the documented Q16 utilization ceiling.
    let governed = desired.map(|demand| demand * 62_259 / 65_536);
    let mut payload_start = [0_u64; 2];
    let mut admitted = [0_u64; 2];
    let mut blocked = [0_u64; 2];
    let mut empty_ticks = [0_u64; 2];
    for id in 1..=6_000_u64 {
        let tick = start + Duration::from_millis(id * 2);
        fixture.drive_for(tick.saturating_duration_since(fixture.at().monotonic));
        let first = usize::try_from(id % 2).unwrap_or_default();
        for index in [first, 1 - first] {
            // Keep both lanes backlogged without letting one fill the shared admission horizon.
            if desired[index] == 0 || fixture.connection.stats().senders[index].queued_packets >= 1
            {
                continue;
            }
            let source = &mut sources[index];
            let media = forwarded(source.sample(fixture.at(), &[0x5a; PAYLOAD_BYTES]), id);
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
            let budget = stats
                .senders
                .iter()
                .map(|sender| sender.allocation.as_bps())
                .sum::<u64>();
            let first_share = budget * u64::from(weights[0]) / u64::from(weights[0] + weights[1]);
            let shares = if first_share > governed[0] {
                [governed[0], (budget - governed[0]).min(governed[1])]
            } else if budget - first_share > governed[1] {
                [(budget - governed[1]).min(governed[0]), governed[1]]
            } else {
                [first_share, budget - first_share]
            };
            for (index, sender) in stats.senders.iter().enumerate() {
                integrated[index] += u128::from(sender.allocation.as_bps()) * dt;
                expected[index] += u128::from(shares[index]) * dt;
                empty_ticks[index] += u64::from(sender.queued_packets == 0);
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
    let payload_end = fixture
        .connection
        .stats()
        .senders
        .iter()
        .map(|sender| sender.transmitted_payload_bytes)
        .collect::<Vec<_>>();
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
    let delivered = [0, 1].map(|i| payload_end[i] - payload_start[i]);
    eprintln!(
        "two-sender seed=0x4401 weights={weights:?} desired={desired:?} empty_ticks={empty_ticks:?} admitted={admitted:?} blocked={blocked:?} integrated_allocation={integrated:?} expected={expected:?} payload={delivered:?} end_stats={:?}",
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
    for index in 0..2 {
        if desired[index] > 0 {
            assert_eq!(
                empty_ticks[index], 0,
                "stable sender must remain backlogged"
            );
        }
        assert!(
            integrated[index].abs_diff(expected[index]) * 10 <= expected[index],
            "allocation differs from weighted demand-capped share: {integrated:?} vs {expected:?}"
        );
        let delivered_share = u128::from(delivered[index]) * total;
        let allocation_share = integrated[index] * u128::from(total_payload);
        assert!(
            delivered_share.abs_diff(allocation_share) * 10 <= allocation_share,
            "payload differs from normalized allocation: {delivered:?} vs {integrated:?}"
        );
    }
}
