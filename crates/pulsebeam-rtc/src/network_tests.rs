#![allow(
    clippy::arithmetic_side_effects,
    clippy::disallowed_types,
    clippy::expect_used,
    clippy::panic,
    clippy::print_stderr,
    reason = "the deterministic system harness prints its seed/trace and fails at the first invariant"
)]

use std::time::Duration;

use crate::test_support::{NetworkPolicy, PeerFixture, assert_probe_windows, sample};
use crate::{MediaPayloadBitrate, WriteError};

const PAYLOAD_BYTES: usize = 1_000;

#[test]
fn production_connection_bottleneck_emits_measured_transport_bytes() {
    // Keep packet serialization below the unchanged 60 ms queue-delay target.
    const PAYLOAD_BYTES: usize = 500;
    const SEED: u64 = 0x1122;
    const RATE_BPS: u64 = 100_000;
    const PACKETS: u64 = 4_000;
    let origin = std::time::Instant::now();
    let mut fixture = PeerFixture::connected_at(origin).without_egress_observation();
    fixture.configure_network(
        SEED,
        NetworkPolicy {
            delay: Duration::from_millis(25),
            ..NetworkPolicy::default()
        },
    );
    fixture.configure_bottleneck(RATE_BPS);
    fixture.configure_time_quantum(Duration::from_millis(5));
    fixture.set_desired_bitrate(Some(MediaPayloadBitrate::from_bps(1_000_000)));
    let mid = fixture.sender_mid.clone();
    let started = fixture.at();
    fixture.drive_for(started.saturating_duration_since(fixture.at()));
    let mut emitted_windows = std::collections::VecDeque::from([(fixture.at(), 0_u64, 0_u64)]);
    let mut max_probe_percent = 0_u64;
    let mut measured_windows = 0_u64;
    for id in 1..=PACKETS {
        let at = started + Duration::from_millis(id * 40);
        fixture.drive_for(at.saturating_duration_since(fixture.at()));
        let media = sample(fixture.at(), id, PAYLOAD_BYTES, 48_000);
        match fixture.connection.write_rtp(fixture.at(), &mid, media) {
            Ok(())
            | Err(pulsebeam_rtc::RejectedRtp {
                reason: WriteError::WouldBlock,
                ..
            }) => {}
            Err(error) => panic!("production admission at {id}: {error:?}"),
        }
        fixture.drive_for(Duration::from_millis(1));
        let totals = fixture.connection.stats().connection;
        emitted_windows.push_back((
            fixture.at(),
            totals.transmitted_rtp_bytes,
            totals.transmitted_padding_bytes,
        ));
        while emitted_windows.get(1).is_some_and(|(at, _, _)| {
            fixture.at().saturating_duration_since(*at) >= Duration::from_secs(5)
        }) {
            emitted_windows.pop_front();
        }
        if let Some((start, rtp, padding)) = emitted_windows.front()
            && fixture.at().saturating_duration_since(*start) >= Duration::from_secs(5)
        {
            measured_windows += 1;
            let emitted = totals.transmitted_rtp_bytes.saturating_sub(*rtp);
            let probes = totals.transmitted_padding_bytes.saturating_sub(*padding);
            let total = emitted.saturating_add(probes);
            max_probe_percent =
                max_probe_percent.max(probes.saturating_mul(100).checked_div(total).unwrap_or(0));
            assert!(probes <= emitted / 19 + 48 * 1024);
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
        stats.connection.bandwidth_estimate.as_bps(),
        p99.as_millis()
    );
    assert!(
        sojourn.len() >= 1_000,
        "insufficient emitted transport samples"
    );
    let elapsed_micros = fixture.at().duration_since(started).as_micros();
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
fn production_capacity_steps_recover_stable_transport_service() {
    let mut fixture = PeerFixture::connected_with_senders(2).without_egress_observation();
    fixture.configure_network(
        0x6601,
        NetworkPolicy {
            delay: Duration::from_millis(25),
            ..NetworkPolicy::default()
        },
    );
    fixture.configure_bottleneck(2_000_000);
    fixture.configure_time_quantum(Duration::from_millis(2));
    fixture.set_desired_bitrate(Some(MediaPayloadBitrate::from_bps(8_000_000)));
    let senders = fixture.senders.clone();
    let start = fixture.at();
    let mut budgets = Vec::with_capacity(3);
    for id in 1..=18_000_u64 {
        let tick = start + Duration::from_millis(id * 2);
        fixture.drive_for(tick.saturating_duration_since(fixture.at()));
        if id == 6_000 {
            fixture.set_bottleneck_rate(1_000_000);
        }
        if id == 12_000 {
            fixture.set_bottleneck_rate(2_000_000);
        }
        if id % 6_000 == 0 {
            let stats = fixture.connection.stats();
            budgets.push(stats.connection.bandwidth_estimate.as_bps());
        }
        for sender in &senders {
            let media = sample(fixture.at(), id, PAYLOAD_BYTES, 48_000);
            match fixture.connection.write_rtp(fixture.at(), sender, media) {
                Ok(())
                | Err(pulsebeam_rtc::RejectedRtp {
                    reason: WriteError::WouldBlock,
                    ..
                }) => {}
                Err(error) => panic!("step admission: {error:?}"),
            }
        }
    }
    fixture.drive_for(Duration::from_secs(2));
    assert!(
        fixture
            .connection
            .stats()
            .senders
            .iter()
            .all(|sender| sender.transmitted_payload_bytes > 0)
    );
    assert!(
        budgets[1] < budgets[0],
        "native budget contracts without caller target changes: {budgets:?}"
    );
    assert!(
        budgets[2] > budgets[1],
        "native budget recovers without caller target changes: {budgets:?}"
    );
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
        assert!(u128::from(delivered) * 100 <= u128::from(service) * 115);
    }
}

#[test]
fn production_homogeneous_shared_bottleneck_competition() {
    let shared = std::rc::Rc::new(std::cell::RefCell::new(None));
    let mut flows = vec![
        PeerFixture::connected().without_egress_observation(),
        PeerFixture::connected().without_egress_observation(),
    ];
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
        flow.set_desired_bitrate(Some(MediaPayloadBitrate::from_bps(4_000_000)));
    }
    let start = flows.iter().map(PeerFixture::at).max().unwrap();
    for id in 1..=6_000_u64 {
        let tick = start + Duration::from_millis(id * 2);
        let first = usize::try_from(id % 2).unwrap_or_default();
        for index in [first, 1 - first] {
            let flow = &mut flows[index];
            flow.drive_for(tick.saturating_duration_since(flow.at()));
            let media = sample(flow.at(), id, PAYLOAD_BYTES, 48_000);
            match flow
                .connection
                .write_rtp(flow.at(), &flow.sender_mid.clone(), media)
                .map_err(|rejected| rejected.reason)
            {
                Ok(()) | Err(WriteError::WouldBlock) => {}
                Err(error) => panic!("shared-flow admission: {error:?}"),
            }
        }
    }
    for flow in &mut flows {
        flow.drive_for((start + Duration::from_secs(12)).saturating_duration_since(flow.at()));
    }
    let end = flows.iter().map(PeerFixture::at).min().unwrap();
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
            .max(flows[1].at())
            .duration_since(flows[0].at().min(flows[1].at())),
        flows
            .iter()
            .map(|flow| flow
                .connection
                .stats()
                .connection
                .bandwidth_estimate
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
