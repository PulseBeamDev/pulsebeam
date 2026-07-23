//! 1:1 video call: the simplest topology, and the one real users notice
//! quality regressions in fastest -- a friend/family or two-person business
//! call. The bar here is the highest in the suite: on anything short of a
//! genuinely bad link, the call should look and feel perfect.

use super::support::{self, Outage};
use pulsebeam_agent::{MediaKind, TransceiverDirection};
use std::time::Duration;

/// A clean home broadband call must reach and hold the full layer almost
/// the entire time -- there is no contention, no adversarial impairment,
/// nothing to justify sitting at a lower tier. This is the suite's highest
/// bar and doubles as a sanity check: if this fails, something is broken
/// everywhere, not just under duress.
#[test]
fn clean_broadband_call_holds_full_layer() {
    let _guard = support::test_guard();
    const TICK: Duration = Duration::from_millis(1);
    const WARMUP: Duration = Duration::from_secs(10);
    const SOAK: Duration = Duration::from_secs(90);

    let mut sim = turmoil::Builder::new()
        .simulation_duration(WARMUP + SOAK + Duration::from_secs(10))
        .tick_duration(TICK)
        .min_message_latency(support::BROADBAND.min_latency)
        .max_message_latency(support::BROADBAND.max_latency)
        .fail_rate(support::BROADBAND.fail_rate)
        .rng_seed(support::seed("one_to_one_clean_broadband"))
        .build();

    let subnet = crate::tests::common::reserve_subnet();
    let server_ip = crate::tests::common::subnet_ip(subnet, 1);
    let sender_ip = crate::tests::common::subnet_ip(subnet, 2);
    let receiver_ip = crate::tests::common::subnet_ip(subnet, 3);

    support::spawn_sfu(&mut sim, server_ip);
    let done = tokio_util::sync::CancellationToken::new();
    support::spawn_publisher(
        &mut sim,
        sender_ip,
        server_ip,
        "one-to-one-clean",
        done.clone(),
    );

    sim.client(receiver_ip, async move {
        let _done = done.drop_guard();
        let mut client =
            crate::tests::common::client::SimClientBuilder::bind(receiver_ip, server_ip)
                .await?
                .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
                .connect("one-to-one-clean")
                .await?;

        support::warmup_until_all_flowing(&mut client, WARMUP, 1).await?;
        client.ctx.mark_qoe_baseline();
        client.drive_for(SOAK).await?;

        client.ctx.assert_all_streams_decodable();
        client
            .ctx
            .assert_max_freeze_under(Duration::from_millis(500));
        client.ctx.assert_min_qoe_score(92.0);

        Ok(())
    });

    crate::tests::common::run_sim_or_timeout(&mut sim, Duration::from_secs(5 * 60))
        .unwrap_or_else(|error| panic!("simulation failed: {error}"));
}

/// The single most common real-world complaint: a call over a home WiFi
/// network that's also carrying other household traffic. Real jitter, real
/// occasional loss, sustained for a while -- must stay clearly watchable,
/// not just technically alive.
#[test]
fn congested_home_wifi_call_stays_clearly_watchable() {
    let _guard = support::test_guard();
    const TICK: Duration = Duration::from_millis(1);
    const WARMUP: Duration = Duration::from_secs(15);
    const SOAK: Duration = Duration::from_secs(90);

    let mut sim = turmoil::Builder::new()
        .simulation_duration(WARMUP + SOAK + Duration::from_secs(10))
        .tick_duration(TICK)
        .min_message_latency(support::CONGESTED_WIFI.min_latency)
        .max_message_latency(support::CONGESTED_WIFI.max_latency)
        .fail_rate(support::CONGESTED_WIFI.fail_rate)
        .rng_seed(support::seed("one_to_one_congested_wifi"))
        .build();

    let subnet = crate::tests::common::reserve_subnet();
    let server_ip = crate::tests::common::subnet_ip(subnet, 1);
    let sender_ip = crate::tests::common::subnet_ip(subnet, 2);
    let receiver_ip = crate::tests::common::subnet_ip(subnet, 3);

    support::spawn_sfu(&mut sim, server_ip);
    let done = tokio_util::sync::CancellationToken::new();
    support::spawn_publisher(
        &mut sim,
        sender_ip,
        server_ip,
        "one-to-one-congested",
        done.clone(),
    );

    sim.client(receiver_ip, async move {
        let _done = done.drop_guard();
        let mut client =
            crate::tests::common::client::SimClientBuilder::bind(receiver_ip, server_ip)
                .await?
                .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
                .connect("one-to-one-congested")
                .await?;

        support::warmup_until_all_flowing(&mut client, WARMUP, 1).await?;
        client.ctx.mark_qoe_baseline();
        client.drive_for(SOAK).await?;

        client.ctx.assert_all_streams_decodable();
        // A brief stutter is tolerable on real WiFi; multi-second blackouts
        // are not, for a call this ordinary.
        client.ctx.assert_max_freeze_under(Duration::from_secs(1));
        client.ctx.assert_min_qoe_score(78.0);

        Ok(())
    });

    crate::tests::common::run_sim_or_timeout(&mut sim, Duration::from_secs(5 * 60))
        .unwrap_or_else(|error| panic!("simulation failed: {error}"));
}

/// A phone call that walks out of WiFi range mid-conversation and hands off
/// to cellular: a real, brief, total outage in the middle of an otherwise
/// fine call. The property under test is recovery, not survival during --
/// the call must come back promptly and cleanly, not limp along or need a
/// human to notice and reconnect.
#[test]
fn call_recovers_promptly_after_a_network_handoff() {
    let _guard = support::test_guard();
    const TICK: Duration = Duration::from_millis(1);
    // Connection setup itself (TCP-based HTTP signaling, ICE, DTLS) rides
    // out the same real loss, so it needs a generous, condition-checked
    // budget rather than a tight fixed one -- matching the pattern used
    // elsewhere in this suite for lossy-profile warmups.
    const WARMUP: Duration = Duration::from_secs(30);
    const OUTAGE: Duration = Duration::from_secs(3);
    const RECOVERY_GRACE: Duration = Duration::from_secs(2);
    const SETTLE: Duration = Duration::from_secs(30);

    let mut sim = turmoil::Builder::new()
        .simulation_duration(WARMUP + OUTAGE + RECOVERY_GRACE + SETTLE + Duration::from_secs(10))
        .tick_duration(TICK)
        .min_message_latency(support::CELLULAR_LTE.min_latency)
        .max_message_latency(support::CELLULAR_LTE.max_latency)
        .fail_rate(support::CELLULAR_LTE.fail_rate)
        .rng_seed(support::seed("one_to_one_handoff"))
        .build();

    let subnet = crate::tests::common::reserve_subnet();
    let server_ip = crate::tests::common::subnet_ip(subnet, 1);
    let sender_ip = crate::tests::common::subnet_ip(subnet, 2);
    let receiver_ip = crate::tests::common::subnet_ip(subnet, 3);

    support::spawn_sfu(&mut sim, server_ip);
    let done = tokio_util::sync::CancellationToken::new();
    support::spawn_publisher(
        &mut sim,
        sender_ip,
        server_ip,
        "one-to-one-handoff",
        done.clone(),
    );

    sim.client(receiver_ip, async move {
        let _done = done.drop_guard();
        let mut client =
            crate::tests::common::client::SimClientBuilder::bind(receiver_ip, server_ip)
                .await?
                .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
                .connect("one-to-one-handoff")
                .await?;

        support::warmup_until_all_flowing(&mut client, WARMUP, 1).await?;
        assert!(
            client.ctx.total_received_media_bytes() > 0,
            "call did not establish during warmup"
        );

        support::drive_through_downlink_outage(
            &mut client,
            receiver_ip,
            server_ip,
            Outage::Partition(OUTAGE),
        )
        .await?;

        // The recovery bound itself is the assertion: a genuine handoff
        // costs the outage itself plus one keyframe/NACK round trip, not
        // multiple additional seconds of silence while the allocator
        // re-decides from scratch. Nothing can arrive during a full
        // partition, so the bound must cover at least `OUTAGE` -- it is not
        // a "how fast did it recover" budget on its own.
        client.drive_for(RECOVERY_GRACE).await?;
        client.ctx.assert_max_freeze_under(OUTAGE + RECOVERY_GRACE);

        // Measure steady-state quality only after that one-time recovery
        // cost has been absorbed, so it isn't double-counted into the score
        // below.
        client.ctx.mark_qoe_baseline();
        client.drive_for(SETTLE).await?;
        client.ctx.assert_all_streams_decodable();
        client.ctx.assert_min_qoe_score(75.0);

        Ok(())
    });

    crate::tests::common::run_sim_or_timeout(&mut sim, Duration::from_secs(5 * 60))
        .unwrap_or_else(|error| panic!("simulation failed: {error}"));
}
