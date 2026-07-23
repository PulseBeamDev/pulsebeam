//! Teleoperation: a remote-controlled device (robot, drone, remote-driving
//! vehicle) streaming its camera to an operator who is acting on what they
//! see in close to real time. This is the suite's most latency-sensitive
//! use case -- worse than a dropped frame here is a *frozen* frame the
//! operator doesn't know is stale, so every scenario asserts a strict bound
//! on the single worst outage (`assert_max_freeze_under`), not just an
//! averaged QoE score. A teleoperation link is also realistically over
//! cellular or another constrained, lossy connection, not a boardroom's
//! wired network, so these scenarios never run over `BROADBAND`.

use super::support::{self, Outage};
use pulsebeam_agent::{MediaKind, TransceiverDirection};
use std::time::Duration;

/// The baseline teleoperation condition: a moving device on a cellular
/// link, with the periodic brief total outages real cellular connections
/// have (tunnels, dead zones, tower handoffs). An operator cannot tolerate
/// losing the feed for long, so the freeze bound here is the tightest in
/// the suite -- well under what a casual video call would consider fine.
#[test]
fn cellular_link_never_blacks_out_the_control_feed() {
    let _guard = support::test_guard();
    const TICK: Duration = Duration::from_millis(1);
    const WARMUP: Duration = Duration::from_secs(10);
    const OUTAGE_COUNT: usize = 4;
    const OUTAGE_DURATION: Duration = Duration::from_millis(600);
    const BETWEEN_OUTAGES: Duration = Duration::from_secs(15);

    let mut sim = turmoil::Builder::new()
        .simulation_duration(
            WARMUP + (OUTAGE_DURATION + BETWEEN_OUTAGES) * (OUTAGE_COUNT as u32) + Duration::from_secs(10),
        )
        .tick_duration(TICK)
        .min_message_latency(support::CELLULAR_LTE.min_latency)
        .max_message_latency(support::CELLULAR_LTE.max_latency)
        .fail_rate(support::CELLULAR_LTE.fail_rate)
        .rng_seed(support::seed("teleoperation_cellular_control_feed"))
        .build();

    let subnet = crate::tests::common::reserve_subnet();
    let server_ip = crate::tests::common::subnet_ip(subnet, 1);
    let sender_ip = crate::tests::common::subnet_ip(subnet, 2);
    let receiver_ip = crate::tests::common::subnet_ip(subnet, 3);

    support::spawn_sfu(&mut sim, server_ip);
    let done = tokio_util::sync::CancellationToken::new();
    support::spawn_publisher(&mut sim, sender_ip, server_ip, "teleop-cellular", done.clone());

    sim.client(receiver_ip, async move {
        let _done = done.drop_guard();
        let mut client = crate::tests::common::client::SimClientBuilder::bind(receiver_ip, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .connect("teleop-cellular")
            .await?;

        support::warmup_until_all_flowing(&mut client, WARMUP, 1).await?;
        client.ctx.mark_qoe_baseline();

        for outage in 0..OUTAGE_COUNT {
            support::drive_through_downlink_outage(
                &mut client,
                receiver_ip,
                server_ip,
                Outage::Partition(OUTAGE_DURATION),
            )
            .await?;
            client.drive_for(BETWEEN_OUTAGES).await?;
            tracing::debug!(outage, "recovered from a cellular-style outage");
        }

        client.ctx.assert_all_streams_decodable();
        // The outage itself plus one keyframe/NACK round trip to resync --
        // not multiple seconds of the operator flying blind.
        client
            .ctx
            .assert_max_freeze_under(OUTAGE_DURATION + Duration::from_secs(1));
        client.ctx.assert_min_qoe_score(70.0);

        Ok(())
    });

    crate::tests::common::run_sim_or_timeout(&mut sim, Duration::from_secs(5 * 60))
        .unwrap_or_else(|error| panic!("simulation failed: {error}"));
}

/// A device controlled over a satellite backhaul (maritime, remote
/// agriculture, disaster response where terrestrial links are down): high
/// but *stable* latency. Physics, not congestion -- the link must still
/// deliver full detail and, just as importantly for a control feed, must
/// not treat the fixed delay itself as a reason to freeze or stall.
#[test]
fn satellite_backhaul_still_delivers_a_continuous_feed() {
    let _guard = support::test_guard();
    const TICK: Duration = Duration::from_millis(1);
    const WARMUP: Duration = Duration::from_secs(10);
    const RAMP: Duration = Duration::from_secs(30);
    const SOAK: Duration = Duration::from_secs(60);

    let mut sim = turmoil::Builder::new()
        .simulation_duration(WARMUP + RAMP + SOAK + Duration::from_secs(10))
        .tick_duration(TICK)
        .min_message_latency(support::SATELLITE.min_latency)
        .max_message_latency(support::SATELLITE.max_latency)
        .rng_seed(support::seed("teleoperation_satellite_backhaul"))
        .build();

    let subnet = crate::tests::common::reserve_subnet();
    let server_ip = crate::tests::common::subnet_ip(subnet, 1);
    let sender_ip = crate::tests::common::subnet_ip(subnet, 2);
    let receiver_ip = crate::tests::common::subnet_ip(subnet, 3);

    support::spawn_sfu(&mut sim, server_ip);
    let done = tokio_util::sync::CancellationToken::new();
    support::spawn_publisher(&mut sim, sender_ip, server_ip, "teleop-satellite", done.clone());

    sim.client(receiver_ip, async move {
        let _done = done.drop_guard();
        let mut client = crate::tests::common::client::SimClientBuilder::bind(receiver_ip, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .connect("teleop-satellite")
            .await?;

        support::warmup_until_all_flowing(&mut client, WARMUP, 1).await?;
        client.drive_for(RAMP).await?;
        client.ctx.mark_qoe_baseline();
        client.drive_for(SOAK).await?;

        client.ctx.assert_all_streams_decodable();
        client.ctx.assert_max_freeze_under(Duration::from_millis(500));
        client.ctx.assert_min_qoe_score(85.0);

        Ok(())
    });

    crate::tests::common::run_sim_or_timeout(&mut sim, Duration::from_secs(5 * 60))
        .unwrap_or_else(|error| panic!("simulation failed: {error}"));
}

/// The device's own uplink is the constrained, lossy leg (a robot on a weak
/// cell signal pushing video out), not the operator's downlink. The
/// operator must never fully lose the feed even while the robot's own
/// connection is degraded and shedding layers -- a low-resolution feed the
/// operator can still act on beats a dropped one every time.
#[test]
fn constrained_device_uplink_never_fully_drops_the_feed() {
    let _guard = support::test_guard();
    const TICK: Duration = Duration::from_millis(1);
    const WARMUP: Duration = Duration::from_secs(10);
    const CYCLES: usize = 3;
    const LOW_PHASE: Duration = Duration::from_secs(15);
    const RECOVER_PHASE: Duration = Duration::from_secs(20);
    const LOW_UPLINK_BPS: u64 = 220_000;

    let mut sim = turmoil::Builder::new()
        .simulation_duration(WARMUP + (LOW_PHASE + RECOVER_PHASE) * (CYCLES as u32) + Duration::from_secs(10))
        .tick_duration(TICK)
        .min_message_latency(support::CELLULAR_LTE.min_latency)
        .max_message_latency(support::CELLULAR_LTE.max_latency)
        .fail_rate(support::CELLULAR_LTE.fail_rate)
        .rng_seed(support::seed("teleoperation_constrained_uplink"))
        .build();

    let subnet = crate::tests::common::reserve_subnet();
    let server_ip = crate::tests::common::subnet_ip(subnet, 1);
    let sender_ip = crate::tests::common::subnet_ip(subnet, 2);
    let receiver_ip = crate::tests::common::subnet_ip(subnet, 3);

    support::spawn_sfu(&mut sim, server_ip);
    let done = tokio_util::sync::CancellationToken::new();
    support::spawn_publisher(&mut sim, sender_ip, server_ip, "teleop-uplink", done.clone());

    sim.client(receiver_ip, async move {
        let _done = done.drop_guard();
        let mut client = crate::tests::common::client::SimClientBuilder::bind(receiver_ip, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .connect("teleop-uplink")
            .await?;

        support::warmup_until_all_flowing(&mut client, WARMUP, 1).await?;
        client.ctx.mark_qoe_baseline();

        for cycle in 0..CYCLES {
            crate::tests::common::set_uplink_bandwidth(sender_ip, Some(LOW_UPLINK_BPS));
            client.drive_for(LOW_PHASE).await?;
            crate::tests::common::set_uplink_bandwidth(sender_ip, None);
            client.drive_for(RECOVER_PHASE).await?;
            tracing::debug!(cycle, "completed a constrained-uplink cycle");
        }

        client.ctx.assert_all_streams_decodable();
        // Quality may sag to the lowest layer during the constrained
        // phases, but the operator must never be left with nothing.
        client.ctx.assert_max_freeze_under(Duration::from_secs(2));
        client.ctx.assert_min_qoe_score(45.0);

        Ok(())
    });

    crate::tests::common::run_sim_or_timeout(&mut sim, Duration::from_secs(5 * 60))
        .unwrap_or_else(|error| panic!("simulation failed: {error}"));
}
