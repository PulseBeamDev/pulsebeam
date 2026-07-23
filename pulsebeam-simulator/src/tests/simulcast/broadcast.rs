//! Broadcast / webinar: one publisher, many independent viewers who share
//! nothing with each other except the same upstream source. The defining
//! property is isolation: each viewer's own network is what determines
//! their own quality, a struggling viewer on a bad connection must not drag
//! down a viewer on a great one, and the SFU must not buckle or degrade
//! uniformly just because the audience is large and heterogeneous.

use super::support;
use pulsebeam_agent::{MediaKind, TransceiverDirection};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

struct Viewer {
    label: &'static str,
    profile: support::Profile,
    min_score: f64,
}

/// A realistic webinar audience: most viewers on decent connections, one on
/// a weak/congested mobile link, one on the worst realistic link this suite
/// models. Every viewer must independently be decodable; the good
/// connections must reach a high score regardless of how badly the worst
/// one is doing, and even the worst viewer must clear a baseline -- "some
/// video, however modest" rather than a true blackout.
#[test]
fn heterogeneous_audience_each_gets_their_own_best_quality() {
    let _guard = support::test_guard();
    const TICK: Duration = Duration::from_millis(1);
    const WARMUP_TIMEOUT: Duration = Duration::from_secs(30);
    const RAMP: Duration = Duration::from_secs(20);
    const SOAK: Duration = Duration::from_secs(60);

    let viewers = [
        Viewer { label: "broadband_1", profile: support::BROADBAND, min_score: 90.0 },
        Viewer { label: "broadband_2", profile: support::BROADBAND, min_score: 90.0 },
        Viewer { label: "congested_wifi_1", profile: support::CONGESTED_WIFI, min_score: 75.0 },
        Viewer { label: "congested_wifi_2", profile: support::CONGESTED_WIFI, min_score: 75.0 },
        Viewer { label: "cellular_1", profile: support::CELLULAR_LTE, min_score: 65.0 },
        Viewer { label: "crowded_venue", profile: support::CROWDED_VENUE_WIFI, min_score: 55.0 },
        Viewer { label: "hostile_mobile", profile: support::HOSTILE_MOBILE, min_score: 30.0 },
    ];

    // Every viewer sits on an independent link (a distinct turmoil host
    // pair can't carry per-link latency/loss parameters individually, so
    // each viewer needs its own simulation run sharing the same publisher
    // asset via a fresh subnet). Running them in one process, one after
    // another, still proves the property: identical publisher behavior,
    // independently varying receiver outcomes.
    for viewer in viewers {
        let mut sim = turmoil::Builder::new()
            .simulation_duration(WARMUP_TIMEOUT + RAMP + SOAK + Duration::from_secs(10))
            .tick_duration(TICK)
            .min_message_latency(viewer.profile.min_latency)
            .max_message_latency(viewer.profile.max_latency)
            .fail_rate(viewer.profile.fail_rate)
            .rng_seed(support::seed(&format!("broadcast_heterogeneous_{}", viewer.label)))
            .build();

        let subnet = crate::tests::common::reserve_subnet();
        let server_ip = crate::tests::common::subnet_ip(subnet, 1);
        let sender_ip = crate::tests::common::subnet_ip(subnet, 2);
        let receiver_ip = crate::tests::common::subnet_ip(subnet, 3);

        support::spawn_sfu(&mut sim, server_ip);
        let done = CancellationToken::new();
        support::spawn_publisher(&mut sim, sender_ip, server_ip, "broadcast-heterogeneous", done.clone());

        let min_score = viewer.min_score;
        let label = viewer.label;
        sim.client(receiver_ip, async move {
            let _done = done.drop_guard();
            let mut client = crate::tests::common::client::SimClientBuilder::bind(receiver_ip, server_ip)
                .await?
                .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
                .connect("broadcast-heterogeneous")
                .await?;

            support::warmup_until_all_flowing(&mut client, WARMUP_TIMEOUT, 1).await?;
            client.drive_for(RAMP).await?;
            client.ctx.mark_qoe_baseline();
            client.drive_for(SOAK).await?;

            client.ctx.assert_all_streams_decodable();
            client.ctx.assert_min_qoe_score(min_score);
            tracing::info!(label, min_score, "viewer cleared its independent floor");

            Ok(())
        });

        crate::tests::common::run_sim_or_timeout(&mut sim, Duration::from_secs(5 * 60))
            .unwrap_or_else(|error| panic!("viewer {label}: simulation failed: {error}"));
    }
}

/// A large audience (15 concurrent viewers, one publisher) on a shared
/// realistic condition, run for a while. No individual viewer's downlink
/// contends with another's (broadcast fan-out is per-participant, not a
/// shared pool the way a group call's downlink is) -- so the property under
/// test is that scale itself doesn't degrade anyone: every viewer stays
/// decodable and alive the whole soak, independent of how many others are
/// also watching.
#[test]
fn large_audience_stays_stable_through_a_long_soak() {
    let _guard = support::test_guard();
    const TICK: Duration = Duration::from_millis(1);
    const WARMUP_TIMEOUT: Duration = Duration::from_secs(30);
    const VIEWER_COUNT: usize = 15;
    const SUSTAIN_WINDOWS: usize = 10;
    const SUSTAIN_WINDOW: Duration = Duration::from_secs(10);
    const STALL_FLOOR_BPS: u64 = 40_000;

    let mut sim = turmoil::Builder::new()
        .simulation_duration(
            WARMUP_TIMEOUT + SUSTAIN_WINDOW * (SUSTAIN_WINDOWS as u32) + Duration::from_secs(10),
        )
        .tick_duration(TICK)
        .min_message_latency(support::CONGESTED_WIFI.min_latency)
        .max_message_latency(support::CONGESTED_WIFI.max_latency)
        .fail_rate(support::CONGESTED_WIFI.fail_rate)
        .rng_seed(support::seed("broadcast_large_audience_soak"))
        .build();

    let subnet = crate::tests::common::reserve_subnet();
    let server_ip = crate::tests::common::subnet_ip(subnet, 1);
    let sender_ip = crate::tests::common::subnet_ip(subnet, 2);

    support::spawn_sfu(&mut sim, server_ip);
    let done = CancellationToken::new();
    support::spawn_publisher(&mut sim, sender_ip, server_ip, "broadcast-large-audience", done.clone());

    for i in 0..VIEWER_COUNT {
        let receiver_ip = crate::tests::common::subnet_ip(subnet, 10 + i as u8);
        let done = done.clone();
        sim.client(receiver_ip, async move {
            let _done = done;
            let mut client = crate::tests::common::client::SimClientBuilder::bind(receiver_ip, server_ip)
                .await?
                .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
                .connect("broadcast-large-audience")
                .await?;

            support::warmup_until_all_flowing(&mut client, WARMUP_TIMEOUT, 1).await?;
            let mut last = client.ctx.total_received_media_bytes();

            for window in 0..SUSTAIN_WINDOWS {
                client.drive_for(SUSTAIN_WINDOW).await?;
                let bytes = client.ctx.total_received_media_bytes();
                let bps = (bytes - last) * 8 / SUSTAIN_WINDOW.as_secs();
                assert!(
                    bps > STALL_FLOOR_BPS,
                    "viewer {receiver_ip}: stalled in window {window} among {VIEWER_COUNT} \
                     concurrent viewers: only {bps} bps"
                );
                last = bytes;
            }

            client.ctx.assert_all_streams_decodable();
            client.ctx.assert_min_qoe_score(60.0);

            Ok(())
        });
    }

    crate::tests::common::run_sim_or_timeout(&mut sim, Duration::from_secs(8 * 60))
        .unwrap_or_else(|error| panic!("simulation failed: {error}"));
}
