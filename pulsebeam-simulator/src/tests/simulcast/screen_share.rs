//! Screen sharing: one participant broadcasting their desktop, usually to a
//! small group. Unlike a talking-head camera, screen share content is
//! judged on legibility (can you read the text/diagram), not motion
//! smoothness -- a viewer will forgive a lower frame rate far sooner than a
//! blurry, low-resolution share. Two things follow: quality/stability
//! matter more here than anywhere else in the suite, and the underlying
//! content is bursty (a slide change or a scrolled document is a brief
//! demand spike, not a sustained one), which is exactly what
//! `BitrateEstimate::admission_trend_bps`'s multi-second confirmation
//! window (`rtp/monitor.rs`) exists to avoid misreading as a real capacity
//! change.

use super::support;
use pulsebeam_agent::{MediaKind, TransceiverDirection};
use std::time::Duration;

/// A modest-but-adequate link (enough for the full layer, not lavish) must
/// let a screen share reach and *hold* full resolution. Screen share
/// content is exactly what the suite's other tests are not: mostly static
/// with sharp text, so sitting at a lower layer "just in case" is a real
/// legibility regression, not a conservative safe choice.
#[test]
fn high_detail_share_reaches_and_holds_full_resolution() {
    let _guard = support::test_guard();
    const TICK: Duration = Duration::from_millis(1);
    const WARMUP: Duration = Duration::from_secs(10);
    const RAMP: Duration = Duration::from_secs(20);
    const SOAK: Duration = Duration::from_secs(60);

    let mut sim = turmoil::Builder::new()
        .simulation_duration(WARMUP + RAMP + SOAK + Duration::from_secs(10))
        .tick_duration(TICK)
        .min_message_latency(support::BROADBAND.min_latency)
        .max_message_latency(support::BROADBAND.max_latency)
        .rng_seed(support::seed("screen_share_holds_full_resolution"))
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
        "screen-share-full-res",
        done.clone(),
    );

    sim.client(receiver_ip, async move {
        let _done = done.drop_guard();
        let mut client =
            crate::tests::common::client::SimClientBuilder::bind(receiver_ip, server_ip)
                .await?
                .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
                .connect("screen-share-full-res")
                .await?;

        support::warmup_until_all_flowing(&mut client, WARMUP, 1).await?;
        client.drive_for(RAMP).await?;
        client.ctx.mark_qoe_baseline();
        client.drive_for(SOAK).await?;

        client.ctx.assert_all_streams_decodable();
        client
            .ctx
            .assert_max_freeze_under(Duration::from_millis(500));
        // High weight on quality/stability, not just availability: a
        // screen share sitting at half resolution the whole time would
        // still clear a generic floor but fail this one.
        client.ctx.assert_min_qoe_score(93.0);

        Ok(())
    });

    crate::tests::common::run_sim_or_timeout(&mut sim, Duration::from_secs(5 * 60))
        .unwrap_or_else(|error| panic!("simulation failed: {error}"));
}

/// Content-driven demand is bursty: a slide flips, a document scrolls, then
/// it's static again for a while. This mirrors that as a brief bandwidth
/// window opening and closing well inside the admission-trend confirmation
/// window, repeated several times. The property under test is *no
/// thrashing*: a transient window must not be enough to trigger a
/// climb-then-immediately-collapse cycle, which is far more visually
/// disruptive for readable content than just staying put.
#[test]
fn transient_bandwidth_windows_do_not_cause_thrashing() {
    let _guard = support::test_guard();
    const TICK: Duration = Duration::from_millis(1);
    const WARMUP: Duration = Duration::from_secs(10);
    const CYCLES: usize = 5;
    const NARROW_PHASE: Duration = Duration::from_secs(2);
    const OPEN_PHASE: Duration = Duration::from_secs(2);
    const NARROW_BPS: u64 = 300_000;

    let mut sim = turmoil::Builder::new()
        .simulation_duration(
            WARMUP + (NARROW_PHASE + OPEN_PHASE) * (CYCLES as u32) + Duration::from_secs(10),
        )
        .tick_duration(TICK)
        .min_message_latency(support::BROADBAND.min_latency)
        .max_message_latency(support::BROADBAND.max_latency)
        .rng_seed(support::seed("screen_share_transient_windows"))
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
        "screen-share-transients",
        done.clone(),
    );

    sim.client(receiver_ip, async move {
        let _done = done.drop_guard();
        let mut client =
            crate::tests::common::client::SimClientBuilder::bind(receiver_ip, server_ip)
                .await?
                .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
                .connect("screen-share-transients")
                .await?;

        support::warmup_until_all_flowing(&mut client, WARMUP, 1).await?;
        client.ctx.mark_qoe_baseline();

        for cycle in 0..CYCLES {
            crate::tests::common::set_downlink_bandwidth(receiver_ip, Some(NARROW_BPS));
            client.drive_for(NARROW_PHASE).await?;
            crate::tests::common::set_downlink_bandwidth(receiver_ip, None);
            client.drive_for(OPEN_PHASE).await?;
            tracing::debug!(cycle, "completed a transient bandwidth window");
        }

        client.ctx.assert_all_streams_decodable();
        client.ctx.assert_max_freeze_under(Duration::from_secs(1));
        // The floor here is on stability, expressed through the composite
        // score: constant re-tiering tanks the score's stability term even
        // if availability stays fine.
        client.ctx.assert_min_qoe_score(55.0);

        Ok(())
    });

    crate::tests::common::run_sim_or_timeout(&mut sim, Duration::from_secs(5 * 60))
        .unwrap_or_else(|error| panic!("simulation failed: {error}"));
}

/// A presenter's own connection glitches mid-share (laptop moves out of
/// WiFi range, a VPN reconnects) -- a real, brief, total outage. The
/// audience's readability must recover promptly once the link is back, not
/// require someone to notice the share froze and ask the presenter to
/// restart it.
#[test]
fn presenter_glitch_recovers_without_a_restart() {
    let _guard = support::test_guard();
    const TICK: Duration = Duration::from_millis(1);
    const WARMUP: Duration = Duration::from_secs(10);
    const OUTAGE: Duration = Duration::from_secs(2);
    const SETTLE: Duration = Duration::from_secs(30);

    let mut sim = turmoil::Builder::new()
        .simulation_duration(WARMUP + OUTAGE + SETTLE + Duration::from_secs(10))
        .tick_duration(TICK)
        .min_message_latency(support::CONGESTED_WIFI.min_latency)
        .max_message_latency(support::CONGESTED_WIFI.max_latency)
        .rng_seed(support::seed("screen_share_presenter_glitch"))
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
        "screen-share-glitch",
        done.clone(),
    );

    sim.client(receiver_ip, async move {
        let _done = done.drop_guard();
        let mut client =
            crate::tests::common::client::SimClientBuilder::bind(receiver_ip, server_ip)
                .await?
                .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
                .connect("screen-share-glitch")
                .await?;

        support::warmup_until_all_flowing(&mut client, WARMUP, 1).await?;

        support::drive_through_downlink_outage(
            &mut client,
            receiver_ip,
            server_ip,
            support::Outage::Hold(OUTAGE),
        )
        .await?;

        client.ctx.mark_qoe_baseline();
        client.drive_for(SETTLE).await?;

        client.ctx.assert_all_streams_decodable();
        client.ctx.assert_max_freeze_under(Duration::from_secs(3));
        client.ctx.assert_min_qoe_score(85.0);

        Ok(())
    });

    crate::tests::common::run_sim_or_timeout(&mut sim, Duration::from_secs(5 * 60))
        .unwrap_or_else(|error| panic!("simulation failed: {error}"));
}
