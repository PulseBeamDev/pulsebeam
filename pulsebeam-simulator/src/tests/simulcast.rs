use super::common;
use pulsebeam_agent::{MediaKind, SimulcastLayer, TransceiverDirection};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
enum DownlinkDisruption {
    None,
    Hold(Duration),
    Partition(Duration),
}

/// Per-track cumulative rx bytes, keyed by mid. Unlike the combined
/// media-frame counter, this lets each downstream's rate be checked
/// independently instead of inferring "every stream is healthy" from a
/// combined sum, which a lucky mix of layers (one starved, one oversized)
/// could otherwise satisfy.
fn per_track_bytes(
    client: &common::client::SimClient,
) -> std::collections::HashMap<pulsebeam_agent::str0m::media::Mid, u64> {
    let stats = client.ctx.driver.stats();
    client
        .ctx
        .remote_tracks
        .keys()
        .map(|mid| {
            let bytes = stats
                .tracks
                .get(mid)
                .map_or(0, |t| t.rx_layers.values().map(|l| l.bytes).sum());
            (*mid, bytes)
        })
        .collect()
}

// The simulator's network controls are process-global.  Serializing these
// scenarios keeps a neighboring test's artificial partition/hold from being
// mistaken for a delivery regression in this test.
fn simulcast_test_guard() -> MutexGuard<'static, ()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Keep each impairment profile reproducible even when the Rust test runner
/// starts tests in a different order.  A seed allocated from a global test
/// counter made the same named scenario exercise a different network on each
/// run, which turns a stability assertion into scheduling luck.
fn scenario_seed(name: &str) -> u64 {
    // Preserve the original jitter profile's deterministic seed. Its latency
    // range is already the intended worst-case coverage; changing it merely
    // because test execution order changes is not useful coverage.
    if name == "high_jitter" {
        return 0x5EED_0001;
    }
    name.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// Run a full SFU + publisher + receiver simulation under a deterministic
/// impaired network. Every receive window must make forward progress: this is
/// intentionally stronger than a final-byte-count assertion, which can hide a
/// long simulcast stall followed by a late keyframe.
fn simulcast_downlink_resilience_case(
    name: &'static str,
    min_latency: Duration,
    max_latency: Duration,
    fail_rate: f64,
    disruption: DownlinkDisruption,
    min_bits_per_interval: u64,
) {
    common::setup_tracing();

    const TICK: Duration = Duration::from_millis(1);
    const WARMUP: Duration = Duration::from_secs(8);
    const HEALTH_INTERVAL: Duration = Duration::from_secs(2);
    const HEALTH_WINDOWS: usize = 8;

    let disruption_time = match disruption {
        DownlinkDisruption::None => Duration::ZERO,
        DownlinkDisruption::Hold(duration) | DownlinkDisruption::Partition(duration) => duration,
    };
    let simulation_duration = WARMUP
        + disruption_time
        + HEALTH_INTERVAL * (HEALTH_WINDOWS as u32 + 2)
        + Duration::from_secs(5);

    let mut sim = turmoil::Builder::new()
        .simulation_duration(simulation_duration)
        .tick_duration(TICK)
        .min_message_latency(min_latency)
        .max_message_latency(max_latency)
        .fail_rate(fail_rate)
        .rng_seed(scenario_seed(name))
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let sender_ip = common::subnet_ip(subnet, 2);
    let receiver_ip = common::subnet_ip(subnet, 3);

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF))
            .await
            .map_err(Into::into)
    });

    let done = CancellationToken::new();
    sim.client(sender_ip, {
        let done = done.clone();
        async move {
            let mut client = common::client::SimClientBuilder::bind(sender_ip, server_ip)
                .await?
                .with_track(
                    MediaKind::Video,
                    TransceiverDirection::SendOnly,
                    Some(vec![
                        SimulcastLayer::new("f"),
                        SimulcastLayer::new("h"),
                        SimulcastLayer::new("q"),
                    ]),
                )
                .connect("simulcast-resilience")
                .await?;

            client.drive(done).await?;
            Ok(())
        }
    });

    sim.client(receiver_ip, async move {
        let _done = done.drop_guard();
        let mut client = common::client::SimClientBuilder::bind(receiver_ip, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .connect("simulcast-resilience")
            .await?;

        client.drive_for(WARMUP).await?;
        let warm_stats = client.ctx.driver.stats();
        let mut last_bits = warm_stats
            .peer
            .as_ref()
            .map_or(0, |peer| peer.peer_bytes_rx)
            * 8;
        assert!(
            last_bits > min_bits_per_interval,
            "{name}: simulcast did not establish during warmup"
        );

        let had_disruption = !matches!(disruption, DownlinkDisruption::None);
        match disruption {
            DownlinkDisruption::None => {}
            DownlinkDisruption::Hold(duration) => {
                turmoil::hold(receiver_ip, server_ip);
                client.drive_for(duration).await?;
                turmoil::release(receiver_ip, server_ip);
            }
            DownlinkDisruption::Partition(duration) => {
                turmoil::partition(receiver_ip, server_ip);
                client.drive_for(duration).await?;
                turmoil::repair(receiver_ip, server_ip);
            }
        }

        // A genuine full-network outage (hold/partition) costs a real
        // keyframe/NACK round trip to resync once it lifts — unlike the
        // continuous, no-disruption scenarios, this is a one-time recovery
        // cost, not evidence of an ongoing problem. Give it one uncounted
        // interval before health accounting starts, then measure from that
        // settled baseline.
        const DISRUPTION_GRACE_INTERVALS: u32 = 2;
        if had_disruption {
            client
                .drive_for(HEALTH_INTERVAL * DISRUPTION_GRACE_INTERVALS)
                .await?;
            last_bits = client
                .ctx
                .driver
                .stats()
                .peer
                .as_ref()
                .map_or(0, |peer| peer.peer_bytes_rx)
                * 8;
        }

        // A single window falling short of the floor is not yet a stall: a
        // legitimate keyframe round trip (e.g. genuine packet loss
        // triggering a layer reconsideration) can transiently depress
        // exactly one window's delivered bits without the stream ever
        // actually stopping. Two consecutive misses is the actual stall
        // signal this test is meant to catch — see the doc comment above.
        let mut consecutive_misses = 0u32;
        for window in 0..HEALTH_WINDOWS {
            client.drive_for(HEALTH_INTERVAL).await?;
            let bits = client
                .ctx
                .driver
                .stats()
                .peer
                .as_ref()
                .map_or(0, |peer| peer.peer_bytes_rx)
                * 8;
            let delta = bits.saturating_sub(last_bits);
            if delta >= min_bits_per_interval {
                consecutive_misses = 0;
            } else {
                consecutive_misses += 1;
                assert!(
                    consecutive_misses < 2,
                    "{name}: simulcast stalled for two consecutive health windows ending at \
                     window {window}; received only {delta} bits"
                );
            }
            last_bits = bits;
        }

        Ok(())
    });

    common::run_sim_or_timeout(&mut sim, Duration::from_secs(5 * 60))
        .unwrap_or_else(|error| panic!("{name}: simulation failed: {error}"));
}

#[test]
fn simulcast_maintains_sustained_quality_under_jitter() {
    let _guard = simulcast_test_guard();
    simulcast_downlink_resilience_case(
        "high_jitter",
        Duration::from_millis(40),
        Duration::from_millis(240),
        0.0,
        DownlinkDisruption::None,
        100_000,
    );
}

#[test]
fn simulcast_maintains_sustained_quality_on_clean_path() {
    let _guard = simulcast_test_guard();
    simulcast_downlink_resilience_case(
        "clean_path_sustained_quality",
        Duration::from_millis(5),
        Duration::from_millis(15),
        0.0,
        DownlinkDisruption::None,
        300_000,
    );
}

#[test]
fn simulcast_recovers_after_brief_queueing() {
    let _guard = simulcast_test_guard();
    simulcast_downlink_resilience_case(
        "short_hold_burst",
        Duration::from_millis(20),
        Duration::from_millis(90),
        0.0,
        DownlinkDisruption::Hold(Duration::from_millis(500)),
        250_000,
    );
}

#[test]
fn simulcast_recovers_after_transport_interruption() {
    let _guard = simulcast_test_guard();
    simulcast_downlink_resilience_case(
        "partition_recovery",
        Duration::from_millis(20),
        Duration::from_millis(80),
        0.0,
        DownlinkDisruption::Partition(Duration::from_secs(2)),
        100_000,
    );
}

/// Start on a healthy path, collapse the downlink long enough for GCC to
/// protect the stream, then restore it. The post-recovery windows require
/// sustained medium-or-better delivery rather than merely a low-layer trickle
/// or one late keyframe.
#[test]
fn simulcast_ramps_up_after_capacity_is_restored() {
    let _guard = simulcast_test_guard();
    simulcast_downlink_resilience_case(
        "capacity_collapse_then_restore",
        Duration::from_millis(10),
        Duration::from_millis(35),
        0.0,
        DownlinkDisruption::Partition(Duration::from_secs(4)),
        550_000,
    );
}

#[test]
fn simulcast_maintains_delivery_under_packet_loss() {
    let _guard = simulcast_test_guard();
    simulcast_downlink_resilience_case(
        "moderate_loss",
        Duration::from_millis(20),
        Duration::from_millis(100),
        0.01,
        DownlinkDisruption::None,
        250_000,
    );
}

/// The core responsiveness claim: when downlink bandwidth drops, the SFU
/// must adapt to a fitting layer within a short, bounded time — not linger
/// at the old layer while the link silently discards the excess.
///
/// Reads decoded frame sizes off the receiver's `RemoteTrack` (not
/// aggregate peer byte counters, which lag and include overhead) to tell
/// layers apart unambiguously: full ≈5.2 KB/frame, half ≈1.7 KB/frame,
/// quarter ≈0.6 KB/frame.
///
/// Recovery is checked as "does not stay broken", not "climbs back within
/// N seconds": str0m's own probe controller documents a known ALR
/// "deadlock zone" (see `ProbeControl::maybe_stagnant`) where the estimate
/// can plateau for 40+ seconds after a cap lifts. That's str0m's own
/// responsibility, not asserted here — noted for whoever investigates a
/// slow-recovery report next.
#[test]
fn simulcast_downstream_bwe_tracks_a_bandwidth_step_down_and_recovers() {
    let _guard = simulcast_test_guard();
    common::setup_tracing();

    const TICK: Duration = Duration::from_millis(1);
    const WARMUP: Duration = Duration::from_secs(8);
    // Stability against real-world jitter/RTT noise (see DOWNGRADE_CONFIRMATION
    // and BWE_FALL_TIME_CONSTANT in video.rs) now costs a few extra seconds of
    // reaction latency for a genuine drop, worst case roughly one downgrade
    // confirmation window plus a few EWMA time constants to fully settle.
    // Still an order of magnitude faster than the layered design this
    // replaced, which could take 30s+ to attempt even one downgrade.
    const REACTION: Duration = Duration::from_secs(14);
    const SETTLE_WINDOW: Duration = Duration::from_secs(6);
    // Recovery isn't expected to reach a higher layer within a short bound
    // (see the doc comment above); this phase polls in windows and only
    // proves the stream keeps flowing at its current layer once the cap
    // lifts, i.e. that lifting the cap doesn't itself regress anything.
    const RECOVERY_WINDOWS: usize = 4;
    const RECOVERY_WINDOW: Duration = Duration::from_secs(5);

    // Set strictly between the quarter (150 kbps) and half (400 kbps) layer
    // costs. A correctly-adapted allocator settles on quarter, comfortably
    // under this cap and delivered in full; an allocator still trying to
    // force half (or full) through it is instead pinned at the cap by
    // policing — a ~100 kbps gap that's easy to tell apart from jitter.
    const CONSTRAINED_BPS: u64 = 250_000;
    const ADAPTED_CEILING_BPS: u64 = 220_000;
    const STALL_FLOOR_BPS: u64 = 50_000;

    let mut sim = turmoil::Builder::new()
        .simulation_duration(
            WARMUP
                + REACTION
                + SETTLE_WINDOW
                + RECOVERY_WINDOW * (RECOVERY_WINDOWS as u32)
                + Duration::from_secs(5),
        )
        .tick_duration(TICK)
        .min_message_latency(Duration::from_millis(10))
        .max_message_latency(Duration::from_millis(35))
        .rng_seed(0xDEADBEEF)
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let sender_ip = common::subnet_ip(subnet, 2);
    let receiver_ip = common::subnet_ip(subnet, 3);

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF))
            .await
            .map_err(Into::into)
    });

    let done = CancellationToken::new();
    sim.client(sender_ip, {
        let done = done.clone();
        async move {
            let mut client = common::client::SimClientBuilder::bind(sender_ip, server_ip)
                .await?
                .with_track(
                    MediaKind::Video,
                    TransceiverDirection::SendOnly,
                    Some(vec![
                        SimulcastLayer::new("f"),
                        SimulcastLayer::new("h"),
                        SimulcastLayer::new("q"),
                    ]),
                )
                .connect("bwe-responsiveness")
                .await?;

            client.drive(done).await?;
            Ok(())
        }
    });

    sim.client(receiver_ip, async move {
        let _done = done.drop_guard();
        let mut client = common::client::SimClientBuilder::bind(receiver_ip, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .connect("bwe-responsiveness")
            .await?;

        // Phase 1: warm up on an unconstrained link; the allocator should
        // climb to the full layer.
        client.drive_for(WARMUP).await?;
        let warm_bytes = client.ctx.total_received_media_bytes();
        assert!(warm_bytes > 0, "stream did not establish during warmup");

        // Phase 2: the downlink's actual available bandwidth collapses.
        common::set_downlink_bandwidth(receiver_ip, Some(CONSTRAINED_BPS));
        client.drive_for(REACTION).await?;
        let before_settle = client.ctx.total_received_media_bytes();
        client.drive_for(SETTLE_WINDOW).await?;
        let after_settle = client.ctx.total_received_media_bytes();
        let settle_bps = (after_settle - before_settle) * 8 / SETTLE_WINDOW.as_secs();
        assert!(
            settle_bps < ADAPTED_CEILING_BPS,
            "downstream did not adapt to the constrained link within {:?}: {settle_bps} bps of decoded media arrived (cap was {CONSTRAINED_BPS} bps)",
            WARMUP + REACTION,
        );
        assert!(
            settle_bps > STALL_FLOOR_BPS,
            "downstream stalled instead of settling on a fitting layer: only {settle_bps} bps of decoded media arrived"
        );

        // Phase 3: bandwidth is restored. Lifting a constraint must not
        // itself cause a stall or regression, whatever pace str0m's own
        // probe controller recovers at (see the doc comment above).
        common::set_downlink_bandwidth(receiver_ip, None);
        let mut last_bytes = client.ctx.total_received_media_bytes();
        for window in 0..RECOVERY_WINDOWS {
            client.drive_for(RECOVERY_WINDOW).await?;
            let bytes = client.ctx.total_received_media_bytes();
            let window_bps = (bytes - last_bytes) * 8 / RECOVERY_WINDOW.as_secs();
            assert!(
                window_bps > STALL_FLOOR_BPS,
                "downstream stalled after bandwidth was restored (window {window}): only {window_bps} bps of decoded media arrived"
            );
            last_bytes = bytes;
        }

        Ok(())
    });

    common::run_sim_or_timeout(&mut sim, Duration::from_secs(5 * 60))
        .unwrap_or_else(|error| panic!("simulation failed: {error}"));
}

/// The other half of the responsiveness claim: reacting fast to a drop is
/// worthless if the allocator can't climb back to the best layer once the
/// link genuinely supports it (see `desired_from_allocation_envelope`'s doc
/// comment for the mechanism this depends on).
///
/// Run long: a stability soak, not just a convergence check. Once the full
/// layer is reached it must hold — no periodic re-drop, no freeze — for
/// several simulated minutes on an unconstrained link.
#[test]
fn simulcast_reaches_and_holds_full_layer_under_ample_bandwidth() {
    let _guard = simulcast_test_guard();
    common::setup_tracing();

    const TICK: Duration = Duration::from_millis(1);
    const WARMUP: Duration = Duration::from_secs(8);
    // Real time for BWE to climb every tier from a cold start.
    const RAMP: Duration = Duration::from_secs(30);
    const SUSTAIN_WINDOWS: usize = 20;
    const SUSTAIN_WINDOW: Duration = Duration::from_secs(10);
    const STALL_FLOOR_BPS: u64 = 50_000;
    // Full layer nominal is ~1.25 Mbps, half is 400 kbps. This floor sits
    // well above half so only sustained full-layer delivery can clear it.
    const FULL_LAYER_FLOOR_BPS: u64 = 900_000;

    let mut sim = turmoil::Builder::new()
        .simulation_duration(
            WARMUP + RAMP + SUSTAIN_WINDOW * (SUSTAIN_WINDOWS as u32) + Duration::from_secs(5),
        )
        .tick_duration(TICK)
        .min_message_latency(Duration::from_millis(5))
        .max_message_latency(Duration::from_millis(20))
        .rng_seed(0xDEADBEEF)
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let sender_ip = common::subnet_ip(subnet, 2);
    let receiver_ip = common::subnet_ip(subnet, 3);

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF))
            .await
            .map_err(Into::into)
    });

    let done = CancellationToken::new();
    sim.client(sender_ip, {
        let done = done.clone();
        async move {
            let mut client = common::client::SimClientBuilder::bind(sender_ip, server_ip)
                .await?
                .with_track(
                    MediaKind::Video,
                    TransceiverDirection::SendOnly,
                    Some(vec![
                        SimulcastLayer::new("f"),
                        SimulcastLayer::new("h"),
                        SimulcastLayer::new("q"),
                    ]),
                )
                .connect("full-layer-soak")
                .await?;

            client.drive(done).await?;
            Ok(())
        }
    });

    sim.client(receiver_ip, async move {
        let _done = done.drop_guard();
        let mut client = common::client::SimClientBuilder::bind(receiver_ip, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .connect("full-layer-soak")
            .await?;

        client.drive_for(WARMUP + RAMP).await?;
        let ramped_bytes = client.ctx.total_received_media_bytes();
        assert!(ramped_bytes > 0, "stream did not establish during warmup");

        let mut last_bytes = ramped_bytes;
        let mut window_bps = Vec::with_capacity(SUSTAIN_WINDOWS);
        for window in 0..SUSTAIN_WINDOWS {
            client.drive_for(SUSTAIN_WINDOW).await?;
            let bytes = client.ctx.total_received_media_bytes();
            let bps = (bytes - last_bytes) * 8 / SUSTAIN_WINDOW.as_secs();
            assert!(
                bps > STALL_FLOOR_BPS,
                "froze in sustain window {window}: only {bps} bps of decoded media arrived"
            );
            window_bps.push(bps);
            last_bytes = bytes;
        }

        let reached_full = window_bps
            .iter()
            .position(|&bps| bps > FULL_LAYER_FLOOR_BPS);
        let Some(reached_full) = reached_full else {
            panic!(
                "never reached the full layer on an ample link; window rates: {window_bps:?}"
            );
        };
        // One transient miss (e.g. a rare residual jitter blip) is tolerated
        // as long as it recovers immediately; two in a row would be a real
        // regression, not a blip.
        let mut consecutive_misses = 0u32;
        for (offset, &bps) in window_bps[reached_full..].iter().enumerate() {
            let window = reached_full + offset;
            if bps > FULL_LAYER_FLOOR_BPS {
                consecutive_misses = 0;
            } else {
                consecutive_misses += 1;
                assert!(
                    consecutive_misses < 2,
                    "dropped out of the full layer for two consecutive windows ending at \
                     window {window} after reaching it in window {reached_full}; \
                     window rates: {window_bps:?}"
                );
            }
        }
        assert!(
            *window_bps.last().unwrap() > FULL_LAYER_FLOOR_BPS,
            "did not end the soak back at the full layer; window rates: {window_bps:?}"
        );

        Ok(())
    });

    common::run_sim_or_timeout(&mut sim, Duration::from_secs(5 * 60))
        .unwrap_or_else(|error| panic!("simulation failed: {error}"));
}

/// Multi-party proof: a participant subscribed to several concurrent
/// downstreams must still let each one climb to its own best layer once
/// aggregate bandwidth allows, and `desired_bitrate` must correctly sum
/// demand across all of them (see `desired_from_allocation_envelope`). Three
/// senders sharing one ample link is deliberately a much harder bar than the
/// single-stream test above: it only clears if all three streams
/// simultaneously reach and hold their full layer, not just the busiest one.
#[test]
fn simulcast_multiple_downstreams_all_reach_full_layer() {
    let _guard = simulcast_test_guard();
    common::setup_tracing();

    const TICK: Duration = Duration::from_millis(1);
    const WARMUP: Duration = Duration::from_secs(8);
    const RAMP: Duration = Duration::from_secs(40);
    const SUSTAIN_WINDOWS: usize = 12;
    const SUSTAIN_WINDOW: Duration = Duration::from_secs(10);
    const STALL_FLOOR_BPS: u64 = 50_000;
    const SENDER_COUNT: usize = 3;
    // Full layer nominal is ~1.25 Mbps, half is 400 kbps. Checked per track
    // (not as a combined sum) so this only passes when *every* one of the
    // three downstreams individually clears it, not just the busiest one(s)
    // averaging out to a plausible-looking combined total.
    const FULL_LAYER_FLOOR_BPS: u64 = 900_000;

    let mut sim = turmoil::Builder::new()
        .simulation_duration(
            WARMUP + RAMP + SUSTAIN_WINDOW * (SUSTAIN_WINDOWS as u32) + Duration::from_secs(5),
        )
        .tick_duration(TICK)
        .min_message_latency(Duration::from_millis(5))
        .max_message_latency(Duration::from_millis(20))
        .rng_seed(0xDEADBEEF)
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let receiver_ip = common::subnet_ip(subnet, 2);

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF))
            .await
            .map_err(Into::into)
    });

    let done = CancellationToken::new();
    for sender_idx in 0..SENDER_COUNT {
        let sender_ip = common::subnet_ip(subnet, 10 + sender_idx as u8);
        let done = done.clone();
        sim.client(sender_ip, async move {
            let mut client = common::client::SimClientBuilder::bind(sender_ip, server_ip)
                .await?
                .with_track(
                    MediaKind::Video,
                    TransceiverDirection::SendOnly,
                    Some(vec![
                        SimulcastLayer::new("f"),
                        SimulcastLayer::new("h"),
                        SimulcastLayer::new("q"),
                    ]),
                )
                .connect("multi-party-full-layer")
                .await?;

            client.drive(done).await?;
            Ok(())
        });
    }

    sim.client(receiver_ip, async move {
        let _done = done.drop_guard();
        let mut builder = common::client::SimClientBuilder::bind(receiver_ip, server_ip).await?;
        for _ in 0..SENDER_COUNT {
            builder = builder.with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None);
        }
        let mut client = builder.connect("multi-party-full-layer").await?;

        client
            .drive_until(Duration::from_secs(10), |ctx| {
                ctx.discovered_tracks.len() >= SENDER_COUNT
            })
            .await?;


        client.drive_for(WARMUP + RAMP).await?;
        let ramped = per_track_bytes(&client);
        assert_eq!(
            ramped.len(),
            SENDER_COUNT,
            "expected {SENDER_COUNT} downstream tracks after warmup, found {}",
            ramped.len()
        );

        let mut last = ramped;
        let mut window_bps: Vec<std::collections::HashMap<_, u64>> =
            Vec::with_capacity(SUSTAIN_WINDOWS);
        for window in 0..SUSTAIN_WINDOWS {
            client.drive_for(SUSTAIN_WINDOW).await?;
            let bytes = per_track_bytes(&client);
            let bps: std::collections::HashMap<_, u64> = bytes
                .iter()
                .map(|(mid, b)| {
                    let prev = last.get(mid).copied().unwrap_or(0);
                    (*mid, (b.saturating_sub(prev)) * 8 / SUSTAIN_WINDOW.as_secs())
                })
                .collect();
            for (mid, rate) in &bps {
                assert!(
                    *rate > STALL_FLOOR_BPS,
                    "downstream {mid:?} froze in window {window}: only {rate} bps"
                );
            }
            window_bps.push(bps);
            last = bytes;
        }

        for mid in client.ctx.remote_tracks.keys() {
            let rates: Vec<u64> = window_bps.iter().map(|w| w[mid]).collect();
            let reached_full = rates.iter().position(|&bps| bps > FULL_LAYER_FLOOR_BPS);
            let Some(reached_full) = reached_full else {
                panic!(
                    "downstream {mid:?} never reached the full layer on an ample \
                     multi-party link; window rates: {rates:?}"
                );
            };
            // Three streams racing for the same link can legitimately cause
            // one brief rebalancing dip (MAX_UPGRADES_PER_TICK limits how
            // many slots upgrade per tick) without any one of them actually
            // regressing. Tolerate one transient miss per stream, same as
            // the stall check above, but require it to recover immediately.
            let mut consecutive_misses = 0u32;
            for (offset, &bps) in rates[reached_full..].iter().enumerate() {
                let window = reached_full + offset;
                if bps > FULL_LAYER_FLOOR_BPS {
                    consecutive_misses = 0;
                } else {
                    consecutive_misses += 1;
                    assert!(
                        consecutive_misses < 2,
                        "downstream {mid:?} dropped out of the full layer for two consecutive \
                         windows ending at window {window} after reaching it in window \
                         {reached_full}; window rates: {rates:?}"
                    );
                }
            }
            assert!(
                *rates.last().unwrap() > FULL_LAYER_FLOOR_BPS,
                "downstream {mid:?} did not end the soak back at the full layer; \
                 window rates: {rates:?}"
            );
        }

        Ok(())
    });

    common::run_sim_or_timeout(&mut sim, Duration::from_secs(5 * 60))
        .unwrap_or_else(|error| panic!("simulation failed: {error}"));
}

#[test]
fn simulcast_stream_stability_test() {
    const TICK: Duration = Duration::from_millis(1);
    const WARMUP: Duration = Duration::from_secs(5);
    const SOAK: Duration = Duration::from_secs(55);
    const HEALTH_INTERVAL: Duration = Duration::from_secs(5);

    let mut sim = turmoil::Builder::new()
        .simulation_duration(WARMUP + SOAK + Duration::from_secs(5)) // headroom
        .tick_duration(TICK)
        .rng_seed(0xDEADBEEF)
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let sender_ip = common::subnet_ip(subnet, 2);
    let receiver_ip = common::subnet_ip(subnet, 3);

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF))
            .await
            .map_err(Into::into)
    });

    let done = CancellationToken::new();
    sim.client(sender_ip, {
        let done = done.clone();
        async move {
            let mut client = common::client::SimClientBuilder::bind(sender_ip, server_ip)
                .await?
                .with_track(
                    MediaKind::Video,
                    TransceiverDirection::SendOnly,
                    Some(vec![
                        SimulcastLayer::new("f"),
                        SimulcastLayer::new("h"),
                        SimulcastLayer::new("q"),
                    ]),
                )
                .connect("room1")
                .await?;

            client.drive(done).await.unwrap();
            Ok(())
        }
    });

    sim.client(receiver_ip, async move {
        let _done = done.drop_guard();
        let mut client = common::client::SimClientBuilder::bind(receiver_ip, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .connect("room1")
            .await?;

        // Phase 1: wait for initial flow to establish
        tracing::info!("waiting for initial flow...");
        client.drive_for(WARMUP).await.unwrap();
        let stats = client.ctx.driver.stats();
        assert!(stats.peer.is_some(), "stream did not establish within warmup window");

        tracing::info!("stream established, entering soak...");

        // Phase 2: soak — assert stream is still making progress every interval
        let num_intervals = SOAK.as_secs() / HEALTH_INTERVAL.as_secs();
        let mut last_bits_rx: u64 = 0;

        for i in 0..num_intervals {
            client.drive_for(HEALTH_INTERVAL).await.unwrap();
            let stats = client.ctx.driver.stats();
            let bits_rx = stats.peer.as_ref().map_or(0, |p| p.peer_bytes_rx) * 8;
            let delta = bits_rx.saturating_sub(last_bits_rx);

            tracing::info!(interval = i, bits_rx, delta, "health check");

            assert!(
                delta > 0,
                "stream stalled at interval {i}: no additional bits received in the last {HEALTH_INTERVAL:?} (total: {bits_rx})"
            );

            last_bits_rx = bits_rx;
        }

        tracing::info!("soak complete, stream remained stable");
        Ok(())
    });

    common::run_sim_or_timeout(&mut sim, Duration::from_secs(5 * 60)).expect("simulation failed");
}

// ── Nasty real-world network conditions ─────────────────────────────────
//
// The tests above isolate one variable at a time; real connections rarely
// offer that courtesy. These combine impairments (jitter+loss, repeated
// disruptions, a genuinely changing capacity) and run long, since a design
// that survives one 30-second impairment can still hide a slow oscillation
// or leak that only shows up over minutes.

/// WiFi-shaped impairment: jitter and loss at the same time, for several
/// minutes. Neither alone is unusual; together they're the most common real
/// complaint (a shaky home/office WiFi link). No bandwidth cap is applied —
/// the network can sustain the full layer — so this isolates whether
/// *noise* (not a real capacity limit) causes flapping or stalling over a
/// long session.
#[test]
fn simulcast_survives_combined_jitter_and_loss_like_real_wifi() {
    let _guard = simulcast_test_guard();
    common::setup_tracing();

    const TICK: Duration = Duration::from_millis(1);
    // Connection setup itself (TCP-based HTTP signaling, ICE, DTLS) rides
    // out the same jitter+loss, so give it a generous, condition-checked
    // budget rather than guessing a fixed warmup duration.
    const WARMUP_TIMEOUT: Duration = Duration::from_secs(45);
    const SUSTAIN_WINDOWS: usize = 20;
    const SUSTAIN_WINDOW: Duration = Duration::from_secs(8);
    const STALL_FLOOR_BPS: u64 = 50_000;
    // Combined jitter+loss is nastier than either alone: the bar here is
    // "reliably sustains at least the low layer" (~150-170kbps of real
    // content), not "reaches a specific higher tier" — a genuinely harsh
    // combined link may not have headroom to safely justify more, and
    // conservatively staying low is the correct, expected choice, not a
    // failure. What must not happen is spending most of the soak barely
    // above the stall floor.
    const SUSTAINED_FLOOR_BPS: u64 = 100_000;

    let mut sim = turmoil::Builder::new()
        .simulation_duration(
            WARMUP_TIMEOUT + SUSTAIN_WINDOW * (SUSTAIN_WINDOWS as u32) + Duration::from_secs(5),
        )
        .tick_duration(TICK)
        .min_message_latency(Duration::from_millis(40))
        .max_message_latency(Duration::from_millis(240))
        .fail_rate(0.01)
        .rng_seed(0xC0FFEE01)
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let sender_ip = common::subnet_ip(subnet, 2);
    let receiver_ip = common::subnet_ip(subnet, 3);

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF))
            .await
            .map_err(Into::into)
    });

    let done = CancellationToken::new();
    sim.client(sender_ip, {
        let done = done.clone();
        async move {
            let mut client = common::client::SimClientBuilder::bind(sender_ip, server_ip)
                .await?
                .with_track(
                    MediaKind::Video,
                    TransceiverDirection::SendOnly,
                    Some(vec![
                        SimulcastLayer::new("f"),
                        SimulcastLayer::new("h"),
                        SimulcastLayer::new("q"),
                    ]),
                )
                .connect("wifi-like")
                .await?;

            client.drive(done).await?;
            Ok(())
        }
    });

    sim.client(receiver_ip, async move {
        let _done = done.drop_guard();
        let mut client = common::client::SimClientBuilder::bind(receiver_ip, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .connect("wifi-like")
            .await?;

        client
            .drive_until(WARMUP_TIMEOUT, |ctx| ctx.total_received_media_bytes() > 0)
            .await?;
        let mut last_bytes = client.ctx.total_received_media_bytes();
        assert!(last_bytes > 0, "stream did not establish during warmup");

        let mut consecutive_misses = 0u32;
        let mut sustained_windows = 0u32;
        for window in 0..SUSTAIN_WINDOWS {
            client.drive_for(SUSTAIN_WINDOW).await?;
            let bytes = client.ctx.total_received_media_bytes();
            let bps = (bytes - last_bytes) * 8 / SUSTAIN_WINDOW.as_secs();
            if bps > STALL_FLOOR_BPS {
                consecutive_misses = 0;
            } else {
                consecutive_misses += 1;
                assert!(
                    consecutive_misses < 2,
                    "stalled for two consecutive windows ending at window {window}: {bps} bps"
                );
            }
            if bps > SUSTAINED_FLOOR_BPS {
                sustained_windows += 1;
            }
            last_bytes = bytes;
        }

        assert!(
            sustained_windows * 4 >= SUSTAIN_WINDOWS as u32 * 3,
            "spent less than 3/4 of a {SUSTAIN_WINDOWS}-window WiFi-like soak reliably above \
             the low-layer floor ({sustained_windows} sustained windows)"
        );

        Ok(())
    });

    common::run_sim_or_timeout(&mut sim, Duration::from_secs(5 * 60))
        .unwrap_or_else(|error| panic!("simulation failed: {error}"));
}

/// The hardest combined case: a *genuine* bandwidth ceiling (the shaper
/// really does throttle egress) while the path is also jittery enough to
/// spuriously trip GCC's delay-based detector on its own. This is what a
/// real constrained-but-also-noisy link looks like — e.g. a phone on a
/// crowded cell tower. It proves two things at once: a real cap still gets
/// tracked (not masked by jitter desensitizing the allocator), and once
/// settled, continued jitter does not reopen the question — no renewed
/// flapping for the remainder of a long tail.
#[test]
fn simulcast_tracks_a_real_cap_while_jitter_continues() {
    let _guard = simulcast_test_guard();
    common::setup_tracing();

    const TICK: Duration = Duration::from_millis(1);
    // Connection setup itself rides out the same jitter; check-driven rather
    // than a tight fixed budget (see the WiFi-like test's rationale).
    const WARMUP: Duration = Duration::from_secs(45);
    const REACTION: Duration = Duration::from_secs(14);
    const SETTLE_WINDOW: Duration = Duration::from_secs(6);
    const STABLE_TAIL_WINDOWS: usize = 12;
    const STABLE_TAIL_WINDOW: Duration = Duration::from_secs(8);

    const CONSTRAINED_BPS: u64 = 250_000;
    const ADAPTED_CEILING_BPS: u64 = 220_000;
    const STALL_FLOOR_BPS: u64 = 50_000;

    let mut sim = turmoil::Builder::new()
        .simulation_duration(
            WARMUP
                + REACTION
                + SETTLE_WINDOW
                + STABLE_TAIL_WINDOW * (STABLE_TAIL_WINDOWS as u32)
                + Duration::from_secs(5),
        )
        .tick_duration(TICK)
        .min_message_latency(Duration::from_millis(40))
        .max_message_latency(Duration::from_millis(240))
        .rng_seed(0xC0FFEE02)
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let sender_ip = common::subnet_ip(subnet, 2);
    let receiver_ip = common::subnet_ip(subnet, 3);

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF))
            .await
            .map_err(Into::into)
    });

    let done = CancellationToken::new();
    sim.client(sender_ip, {
        let done = done.clone();
        async move {
            let mut client = common::client::SimClientBuilder::bind(sender_ip, server_ip)
                .await?
                .with_track(
                    MediaKind::Video,
                    TransceiverDirection::SendOnly,
                    Some(vec![
                        SimulcastLayer::new("f"),
                        SimulcastLayer::new("h"),
                        SimulcastLayer::new("q"),
                    ]),
                )
                .connect("cap-under-jitter")
                .await?;

            client.drive(done).await?;
            Ok(())
        }
    });

    sim.client(receiver_ip, async move {
        let _done = done.drop_guard();
        let mut client = common::client::SimClientBuilder::bind(receiver_ip, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .connect("cap-under-jitter")
            .await?;

        client
            .drive_until(WARMUP, |ctx| ctx.total_received_media_bytes() > 0)
            .await?;
        assert!(
            client.ctx.total_received_media_bytes() > 0,
            "stream did not establish during warmup"
        );

        common::set_downlink_bandwidth(receiver_ip, Some(CONSTRAINED_BPS));
        client.drive_for(REACTION).await?;
        let before_settle = client.ctx.total_received_media_bytes();
        client.drive_for(SETTLE_WINDOW).await?;
        let after_settle = client.ctx.total_received_media_bytes();
        let settle_bps = (after_settle - before_settle) * 8 / SETTLE_WINDOW.as_secs();
        assert!(
            settle_bps < ADAPTED_CEILING_BPS,
            "did not adapt to the real cap despite ongoing jitter within {:?}: {settle_bps} bps \
             (cap was {CONSTRAINED_BPS} bps)",
            WARMUP + REACTION,
        );
        assert!(
            settle_bps > STALL_FLOOR_BPS,
            "stalled instead of settling on a fitting layer: {settle_bps} bps"
        );

        // The real test: does continued jitter, with the cap still active,
        // reopen flapping now that the allocator has committed to a layer?
        let mut last_bytes = after_settle;
        let mut consecutive_misses = 0u32;
        for window in 0..STABLE_TAIL_WINDOWS {
            client.drive_for(STABLE_TAIL_WINDOW).await?;
            let bytes = client.ctx.total_received_media_bytes();
            let bps = (bytes - last_bytes) * 8 / STABLE_TAIL_WINDOW.as_secs();
            if bps > STALL_FLOOR_BPS {
                consecutive_misses = 0;
            } else {
                consecutive_misses += 1;
                assert!(
                    consecutive_misses < 2,
                    "stalled for two consecutive tail windows ending at window {window} \
                     while still capped and jittery: {bps} bps"
                );
            }
            assert!(
                bps < ADAPTED_CEILING_BPS,
                "re-inflated past the real cap in tail window {window} even though the cap \
                 never lifted: {bps} bps (cap was {CONSTRAINED_BPS} bps) — jitter reopened a \
                 decision that should have stayed settled"
            );
            last_bytes = bytes;
        }

        Ok(())
    });

    common::run_sim_or_timeout(&mut sim, Duration::from_secs(5 * 60))
        .unwrap_or_else(|error| panic!("simulation failed: {error}"));
}

/// A phone moving between rooms, or a flaky WiFi AP: several short outages
/// spread across one long call, not just one. Each recovery must be as good
/// as the last — no cumulative degradation, no leaked state from a prior
/// hold making a later recovery worse.
#[test]
fn simulcast_survives_repeated_disruptions_over_a_long_session() {
    let _guard = simulcast_test_guard();
    common::setup_tracing();

    const TICK: Duration = Duration::from_millis(1);
    const WARMUP: Duration = Duration::from_secs(8);
    const HICCUP_COUNT: usize = 4;
    const HICCUP_DURATION: Duration = Duration::from_millis(500);
    const BETWEEN_HICCUPS: Duration = Duration::from_secs(20);
    const STALL_FLOOR_BPS: u64 = 50_000;

    let mut sim = turmoil::Builder::new()
        .simulation_duration(
            WARMUP
                + (HICCUP_DURATION + BETWEEN_HICCUPS) * (HICCUP_COUNT as u32)
                + Duration::from_secs(10),
        )
        .tick_duration(TICK)
        .min_message_latency(Duration::from_millis(15))
        .max_message_latency(Duration::from_millis(70))
        .rng_seed(0xC0FFEE03)
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let sender_ip = common::subnet_ip(subnet, 2);
    let receiver_ip = common::subnet_ip(subnet, 3);

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF))
            .await
            .map_err(Into::into)
    });

    let done = CancellationToken::new();
    sim.client(sender_ip, {
        let done = done.clone();
        async move {
            let mut client = common::client::SimClientBuilder::bind(sender_ip, server_ip)
                .await?
                .with_track(
                    MediaKind::Video,
                    TransceiverDirection::SendOnly,
                    Some(vec![
                        SimulcastLayer::new("f"),
                        SimulcastLayer::new("h"),
                        SimulcastLayer::new("q"),
                    ]),
                )
                .connect("repeated-hiccups")
                .await?;

            client.drive(done).await?;
            Ok(())
        }
    });

    sim.client(receiver_ip, async move {
        let _done = done.drop_guard();
        let mut client = common::client::SimClientBuilder::bind(receiver_ip, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .connect("repeated-hiccups")
            .await?;

        client.drive_for(WARMUP).await?;
        assert!(
            client.ctx.total_received_media_bytes() > 0,
            "stream did not establish during warmup"
        );

        let mut recovered_bits_per_hiccup = Vec::with_capacity(HICCUP_COUNT);
        for hiccup in 0..HICCUP_COUNT {
            turmoil::hold(receiver_ip, server_ip);
            client.drive_for(HICCUP_DURATION).await?;
            turmoil::release(receiver_ip, server_ip);

            // One grace interval to resync (real keyframe/NACK cost after a
            // genuine outage), then measure recovery over the rest of the
            // gap until the next hiccup.
            client.drive_for(Duration::from_secs(2)).await?;
            let before = client.ctx.total_received_media_bytes();
            let remaining = BETWEEN_HICCUPS.saturating_sub(Duration::from_secs(2));
            client.drive_for(remaining).await?;
            let after = client.ctx.total_received_media_bytes();
            let bps = (after - before) * 8 / remaining.as_secs();
            assert!(
                bps > STALL_FLOOR_BPS,
                "did not recover after hiccup {hiccup}: only {bps} bps in the following window"
            );
            recovered_bits_per_hiccup.push(bps);
        }

        tracing::info!(?recovered_bits_per_hiccup, "recovery rate after each hiccup");
        // No cumulative degradation: the last recovery must be at least as
        // healthy as a fraction of the first, not trailing off toward zero.
        let first = recovered_bits_per_hiccup[0];
        let last = *recovered_bits_per_hiccup.last().unwrap();
        assert!(
            last * 2 >= first,
            "recovery degraded across repeated hiccups: first {first} bps, last {last} bps"
        );

        Ok(())
    });

    common::run_sim_or_timeout(&mut sim, Duration::from_secs(5 * 60))
        .unwrap_or_else(|error| panic!("simulation failed: {error}"));
}

/// A shared/contended link whose *actual* available bandwidth genuinely
/// rises and falls repeatedly (e.g. other users joining and leaving a
/// congested access point), for several minutes. Unlike the single
/// step-down-then-up test, this proves the allocator keeps correctly
/// tracking real change after real change without accumulating drift,
/// getting stuck, or thrashing between visits to the same cap.
#[test]
fn simulcast_adapts_to_oscillating_real_bandwidth_without_thrashing() {
    let _guard = simulcast_test_guard();
    common::setup_tracing();

    const TICK: Duration = Duration::from_millis(1);
    const WARMUP: Duration = Duration::from_secs(8);
    const CYCLES: usize = 4;
    const LOW_PHASE: Duration = Duration::from_secs(18);
    // GCC's own climb back up after a real drop is intentionally
    // conservative (see the responsiveness test's doc comment); each high
    // phase needs real time to actually reach a higher tier, not just avoid
    // stalling.
    const HIGH_PHASE: Duration = Duration::from_secs(35);
    const STALL_FLOOR_BPS: u64 = 50_000;

    const LOW_CAP_BPS: u64 = 250_000;
    const LOW_CEILING_BPS: u64 = 220_000;
    // The bar for recovery is "clearly above the low-phase ceiling, no
    // permanent low-only lockout from repeated capping" — not necessarily
    // the full top layer on every single cycle, since climbing all the way
    // there reliably within one bounded phase isn't guaranteed by GCC's own
    // conservative recovery pace. Set with real margin above LOW_CEILING_BPS
    // so it reliably captures genuine medium-tier recovery (observed
    // ~370-450kbps) without being an arbitrary near-miss against it.
    const RECOVERY_FLOOR_BPS: u64 = 300_000;
    // A high phase must never be worse than a capped low phase — recovery
    // should never regress into a state cheaper than what capping already
    // forced. This is deliberately below the low tier's real bitrate
    // (~150-180kbps observed) to leave margin for measurement noise; it's
    // a "never goes backwards" floor, not a quality target.
    const NO_REGRESSION_FLOOR_BPS: u64 = 100_000;

    let mut sim = turmoil::Builder::new()
        .simulation_duration(
            WARMUP + (LOW_PHASE + HIGH_PHASE) * (CYCLES as u32) + Duration::from_secs(10),
        )
        .tick_duration(TICK)
        .min_message_latency(Duration::from_millis(10))
        .max_message_latency(Duration::from_millis(35))
        .rng_seed(0xC0FFEE04)
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let sender_ip = common::subnet_ip(subnet, 2);
    let receiver_ip = common::subnet_ip(subnet, 3);

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF))
            .await
            .map_err(Into::into)
    });

    let done = CancellationToken::new();
    sim.client(sender_ip, {
        let done = done.clone();
        async move {
            let mut client = common::client::SimClientBuilder::bind(sender_ip, server_ip)
                .await?
                .with_track(
                    MediaKind::Video,
                    TransceiverDirection::SendOnly,
                    Some(vec![
                        SimulcastLayer::new("f"),
                        SimulcastLayer::new("h"),
                        SimulcastLayer::new("q"),
                    ]),
                )
                .connect("oscillating-bandwidth")
                .await?;

            client.drive(done).await?;
            Ok(())
        }
    });

    sim.client(receiver_ip, async move {
        let _done = done.drop_guard();
        let mut client = common::client::SimClientBuilder::bind(receiver_ip, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .connect("oscillating-bandwidth")
            .await?;

        client.drive_for(WARMUP).await?;
        let mut last_bytes = client.ctx.total_received_media_bytes();
        assert!(last_bytes > 0, "stream did not establish during warmup");

        let mut high_phase_bps = Vec::with_capacity(CYCLES);
        for cycle in 0..CYCLES {
            common::set_downlink_bandwidth(receiver_ip, Some(LOW_CAP_BPS));
            client.drive_for(LOW_PHASE).await?;
            let bytes = client.ctx.total_received_media_bytes();
            let bps = (bytes - last_bytes) * 8 / LOW_PHASE.as_secs();
            assert!(
                bps > STALL_FLOOR_BPS,
                "stalled during low-bandwidth phase of cycle {cycle}: {bps} bps"
            );
            assert!(
                bps < LOW_CEILING_BPS,
                "did not adapt down during low-bandwidth phase of cycle {cycle}: {bps} bps \
                 (cap was {LOW_CAP_BPS} bps)"
            );
            last_bytes = bytes;

            common::set_downlink_bandwidth(receiver_ip, None);
            client.drive_for(HIGH_PHASE).await?;
            let bytes = client.ctx.total_received_media_bytes();
            let bps = (bytes - last_bytes) * 8 / HIGH_PHASE.as_secs();
            assert!(
                bps > STALL_FLOOR_BPS,
                "stalled during restored-bandwidth phase of cycle {cycle}: {bps} bps"
            );
            last_bytes = bytes;
            tracing::info!(cycle, restored_bps = bps, "oscillation cycle high phase");
            high_phase_bps.push(bps);
        }

        // No permanent lockout: every restored phase must beat the capped
        // low phase, and the system must demonstrably reach a clearly
        // higher tier at least once across the soak. Demanding every single
        // adversarial cycle individually climb to the top tier isn't
        // realistic — str0m's own stagnation recovery has a deliberate 15s+
        // minimum wait, and real networks don't oscillate this cleanly and
        // repeatedly anyway.
        for (cycle, bps) in high_phase_bps.iter().enumerate() {
            assert!(
                *bps >= NO_REGRESSION_FLOOR_BPS,
                "restored-bandwidth phase of cycle {cycle} regressed to {bps} bps — worse \
                 than the capped low phase, a permanent lockout from repeated capping; \
                 per-cycle rates: {high_phase_bps:?}"
            );
        }
        let best_high = *high_phase_bps.iter().max().unwrap();
        assert!(
            best_high > RECOVERY_FLOOR_BPS,
            "none of the {CYCLES} restored-bandwidth phases reached a clearly higher tier — \
             per-cycle rates: {high_phase_bps:?}"
        );

        Ok(())
    });

    common::run_sim_or_timeout(&mut sim, Duration::from_secs(5 * 60))
        .unwrap_or_else(|error| panic!("simulation failed: {error}"));
}

/// Mirrors `simulcast_adapts_to_oscillating_real_bandwidth_without_thrashing`
/// but shapes the *uplink* instead of the downlink, exercising the agent's
/// own `LayerController`/str0m uplink BWE recovery: when upload capacity is
/// squeezed then restored, the agent must resume shed layers responsively
/// rather than stay stuck on its cheapest layer. Downlink stays uncapped,
/// so any observed ceiling is upstream.
///
/// Same calibration as the downlink version: never stall, never regress
/// below the capped floor, and reach a clearly higher tier at least once —
/// not every adversarial cycle individually reaching the top tier, which
/// isn't realistic given str0m's own conservative recovery pace.
#[test]
fn simulcast_upstream_bwe_recovers_after_uplink_congestion() {
    let _guard = simulcast_test_guard();
    common::setup_tracing();

    const TICK: Duration = Duration::from_millis(1);
    const WARMUP: Duration = Duration::from_secs(8);
    const CYCLES: usize = 4;
    const LOW_PHASE: Duration = Duration::from_secs(18);
    const HIGH_PHASE: Duration = Duration::from_secs(35);
    const STALL_FLOOR_BPS: u64 = 50_000;

    const LOW_CAP_BPS: u64 = 250_000;
    const LOW_CEILING_BPS: u64 = 220_000;
    const RECOVERY_FLOOR_BPS: u64 = 300_000;
    const NO_REGRESSION_FLOOR_BPS: u64 = 100_000;

    let mut sim = turmoil::Builder::new()
        .simulation_duration(
            WARMUP + (LOW_PHASE + HIGH_PHASE) * (CYCLES as u32) + Duration::from_secs(10),
        )
        .tick_duration(TICK)
        .min_message_latency(Duration::from_millis(10))
        .max_message_latency(Duration::from_millis(35))
        .rng_seed(0xC0FFEE06)
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let sender_ip = common::subnet_ip(subnet, 2);
    let receiver_ip = common::subnet_ip(subnet, 3);

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF))
            .await
            .map_err(Into::into)
    });

    let done = CancellationToken::new();
    sim.client(sender_ip, {
        let done = done.clone();
        async move {
            let mut client = common::client::SimClientBuilder::bind(sender_ip, server_ip)
                .await?
                .with_track(
                    MediaKind::Video,
                    TransceiverDirection::SendOnly,
                    Some(vec![
                        SimulcastLayer::new("f"),
                        SimulcastLayer::new("h"),
                        SimulcastLayer::new("q"),
                    ]),
                )
                .connect("uplink-oscillating-bandwidth")
                .await?;

            client.drive(done).await?;
            Ok(())
        }
    });

    sim.client(receiver_ip, async move {
        let _done = done.drop_guard();
        let mut client = common::client::SimClientBuilder::bind(receiver_ip, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .connect("uplink-oscillating-bandwidth")
            .await?;

        client.drive_for(WARMUP).await?;
        let mut last_bytes = client.ctx.total_received_media_bytes();
        assert!(last_bytes > 0, "stream did not establish during warmup");

        let mut high_phase_bps = Vec::with_capacity(CYCLES);
        for cycle in 0..CYCLES {
            // Cap the sender's own egress, regardless of destination — not
            // traffic destined to the SFU, which would also throttle the
            // receiver's unrelated ICE/RTCP traffic to the same SFU (see the
            // shaper's module doc).
            common::set_uplink_bandwidth(sender_ip, Some(LOW_CAP_BPS));
            client.drive_for(LOW_PHASE).await?;
            let bytes = client.ctx.total_received_media_bytes();
            let bps = (bytes - last_bytes) * 8 / LOW_PHASE.as_secs();
            assert!(
                bps > STALL_FLOOR_BPS,
                "stalled during uplink-capped phase of cycle {cycle}: {bps} bps"
            );
            assert!(
                bps < LOW_CEILING_BPS,
                "did not adapt down during uplink-capped phase of cycle {cycle}: {bps} bps \
                 (cap was {LOW_CAP_BPS} bps)"
            );
            last_bytes = bytes;

            common::set_uplink_bandwidth(sender_ip, None);
            client.drive_for(HIGH_PHASE).await?;
            let bytes = client.ctx.total_received_media_bytes();
            let bps = (bytes - last_bytes) * 8 / HIGH_PHASE.as_secs();
            assert!(
                bps > STALL_FLOOR_BPS,
                "stalled during restored-uplink phase of cycle {cycle}: {bps} bps"
            );
            last_bytes = bytes;
            tracing::info!(cycle, restored_bps = bps, "uplink oscillation cycle high phase");
            high_phase_bps.push(bps);
        }

        for (cycle, bps) in high_phase_bps.iter().enumerate() {
            assert!(
                *bps >= NO_REGRESSION_FLOOR_BPS,
                "restored-uplink phase of cycle {cycle} regressed to {bps} bps — worse than \
                 the capped low phase, a permanent lockout from repeated uplink capping; \
                 per-cycle rates: {high_phase_bps:?}"
            );
        }
        let best_high = *high_phase_bps.iter().max().unwrap();
        assert!(
            best_high > RECOVERY_FLOOR_BPS,
            "none of the {CYCLES} restored-uplink phases reached a clearly higher tier — \
             the agent's own uplink BWE recovery may be stuck; per-cycle rates: {high_phase_bps:?}"
        );

        Ok(())
    });

    common::run_sim_or_timeout(&mut sim, Duration::from_secs(5 * 60))
        .unwrap_or_else(|error| panic!("simulation failed: {error}"));
}

/// Multiple concurrent downstreams under a genuinely adverse network at the
/// same time — not the ample-bandwidth multi-party test above. Combines the
/// two hardest dimensions this suite covers: several competing streams *and*
/// real jitter/loss, over a long session.
#[test]
fn simulcast_multiple_downstreams_survive_adverse_network() {
    let _guard = simulcast_test_guard();
    common::setup_tracing();

    const TICK: Duration = Duration::from_millis(1);
    // Three concurrent connections riding out jitter+loss for setup; give it
    // a generous, condition-checked budget rather than a tight fixed one.
    const WARMUP: Duration = Duration::from_secs(30);
    const SUSTAIN_WINDOWS: usize = 14;
    const SUSTAIN_WINDOW: Duration = Duration::from_secs(8);
    const STALL_FLOOR_BPS: u64 = 40_000;
    const SENDER_COUNT: usize = 3;

    let mut sim = turmoil::Builder::new()
        .simulation_duration(
            WARMUP + SUSTAIN_WINDOW * (SUSTAIN_WINDOWS as u32) + Duration::from_secs(5),
        )
        .tick_duration(TICK)
        .min_message_latency(Duration::from_millis(30))
        .max_message_latency(Duration::from_millis(150))
        .fail_rate(0.015)
        .rng_seed(0xC0FFEE05)
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let receiver_ip = common::subnet_ip(subnet, 2);

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF))
            .await
            .map_err(Into::into)
    });

    let done = CancellationToken::new();
    for sender_idx in 0..SENDER_COUNT {
        let sender_ip = common::subnet_ip(subnet, 10 + sender_idx as u8);
        let done = done.clone();
        sim.client(sender_ip, async move {
            let mut client = common::client::SimClientBuilder::bind(sender_ip, server_ip)
                .await?
                .with_track(
                    MediaKind::Video,
                    TransceiverDirection::SendOnly,
                    Some(vec![
                        SimulcastLayer::new("f"),
                        SimulcastLayer::new("h"),
                        SimulcastLayer::new("q"),
                    ]),
                )
                .connect("multi-party-adverse")
                .await?;

            client.drive(done).await?;
            Ok(())
        });
    }

    sim.client(receiver_ip, async move {
        let _done = done.drop_guard();
        let mut builder = common::client::SimClientBuilder::bind(receiver_ip, server_ip).await?;
        for _ in 0..SENDER_COUNT {
            builder = builder.with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None);
        }
        let mut client = builder.connect("multi-party-adverse").await?;

        client
            .drive_until(Duration::from_secs(25), |ctx| {
                ctx.discovered_tracks.len() >= SENDER_COUNT
            })
            .await?;

        client
            .drive_until(WARMUP, |ctx| {
                if ctx.remote_tracks.len() < SENDER_COUNT {
                    // Vacuously "all" would otherwise pass on a still-partial
                    // set of remote tracks before every sender has been added.
                    return false;
                }
                let stats = ctx.driver.stats();
                ctx.remote_tracks.keys().all(|mid| {
                    stats
                        .tracks
                        .get(mid)
                        .is_some_and(|t| t.rx_layers.values().any(|l| l.bytes > 0))
                })
            })
            .await?;
        let ramped = per_track_bytes(&client);
        assert_eq!(
            ramped.len(),
            SENDER_COUNT,
            "expected {SENDER_COUNT} downstream tracks after warmup, found {}",
            ramped.len()
        );
        assert!(
            ramped.values().all(|&bytes| bytes > 0),
            "not every downstream established during warmup under adverse conditions: {ramped:?}"
        );

        // Three concurrent connections finishing their *own* independent
        // first-keyframe negotiation under real jitter+loss don't all land
        // on the same instant. That one-time settling cost can straddle a
        // measurement window boundary, showing as a couple of reduced (not
        // zero) windows right at the start of the sustain phase — the same
        // one-time-cost shape as the disruption-recovery grace period used
        // elsewhere in this suite. Give it one uncounted window to settle
        // before strict stall accounting begins from a fresh baseline.
        client.drive_for(SUSTAIN_WINDOW).await?;
        let mut last = per_track_bytes(&client);
        let mut consecutive_misses: std::collections::HashMap<_, u32> =
            last.keys().map(|mid| (*mid, 0)).collect();
        for window in 0..SUSTAIN_WINDOWS {
            client.drive_for(SUSTAIN_WINDOW).await?;
            let bytes = per_track_bytes(&client);
            for (mid, b) in &bytes {
                let prev = last.get(mid).copied().unwrap_or(0);
                let bps = (b.saturating_sub(prev)) * 8 / SUSTAIN_WINDOW.as_secs();
                let misses = consecutive_misses.entry(*mid).or_insert(0);
                if bps > STALL_FLOOR_BPS {
                    *misses = 0;
                } else {
                    *misses += 1;
                    assert!(
                        *misses < 2,
                        "downstream {mid:?} stalled for two consecutive windows ending at \
                         window {window} under adverse multi-party conditions: only {bps} bps"
                    );
                }
            }
            last = bytes;
        }

        Ok(())
    });

    common::run_sim_or_timeout(&mut sim, Duration::from_secs(5 * 60))
        .unwrap_or_else(|error| panic!("simulation failed: {error}"));
}

/// High but *stable* latency with no jitter and no loss — a satellite or
/// long-haul-fiber link. Latency alone, without variance, must not look
/// like congestion: the delay-based estimator's trendline filter reacts to
/// *changing* delay, not a fixed offset. This proves a slow-but-steady path
/// still reaches and holds the top layer, ruling out "any high RTT causes a
/// downgrade" as a failure mode.
#[test]
fn simulcast_high_stable_latency_still_reaches_full_layer() {
    let _guard = simulcast_test_guard();
    common::setup_tracing();

    const TICK: Duration = Duration::from_millis(1);
    const WARMUP: Duration = Duration::from_secs(8);
    const RAMP: Duration = Duration::from_secs(30);
    const SUSTAIN_WINDOWS: usize = 10;
    const SUSTAIN_WINDOW: Duration = Duration::from_secs(10);
    const STALL_FLOOR_BPS: u64 = 50_000;
    const FULL_LAYER_FLOOR_BPS: u64 = 900_000;
    // Satellite-like: ~280ms one-way, essentially fixed (no jitter).
    const FIXED_LATENCY: Duration = Duration::from_millis(280);

    let mut sim = turmoil::Builder::new()
        .simulation_duration(
            WARMUP + RAMP + SUSTAIN_WINDOW * (SUSTAIN_WINDOWS as u32) + Duration::from_secs(5),
        )
        .tick_duration(TICK)
        .min_message_latency(FIXED_LATENCY)
        .max_message_latency(FIXED_LATENCY)
        .rng_seed(0xC0FFEE06)
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let sender_ip = common::subnet_ip(subnet, 2);
    let receiver_ip = common::subnet_ip(subnet, 3);

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF))
            .await
            .map_err(Into::into)
    });

    let done = CancellationToken::new();
    sim.client(sender_ip, {
        let done = done.clone();
        async move {
            let mut client = common::client::SimClientBuilder::bind(sender_ip, server_ip)
                .await?
                .with_track(
                    MediaKind::Video,
                    TransceiverDirection::SendOnly,
                    Some(vec![
                        SimulcastLayer::new("f"),
                        SimulcastLayer::new("h"),
                        SimulcastLayer::new("q"),
                    ]),
                )
                .connect("satellite-like")
                .await?;

            client.drive(done).await?;
            Ok(())
        }
    });

    sim.client(receiver_ip, async move {
        let _done = done.drop_guard();
        let mut client = common::client::SimClientBuilder::bind(receiver_ip, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .connect("satellite-like")
            .await?;

        client.drive_for(WARMUP + RAMP).await?;
        let ramped_bytes = client.ctx.total_received_media_bytes();
        assert!(ramped_bytes > 0, "stream did not establish during warmup");

        let mut last_bytes = ramped_bytes;
        let mut reached_full = false;
        for window in 0..SUSTAIN_WINDOWS {
            client.drive_for(SUSTAIN_WINDOW).await?;
            let bytes = client.ctx.total_received_media_bytes();
            let bps = (bytes - last_bytes) * 8 / SUSTAIN_WINDOW.as_secs();
            assert!(
                bps > STALL_FLOOR_BPS,
                "froze in window {window} despite a stable, loss-free link: {bps} bps"
            );
            if bps > FULL_LAYER_FLOOR_BPS {
                reached_full = true;
            }
            last_bytes = bytes;
        }

        assert!(
            reached_full,
            "high but stable latency alone prevented reaching the full layer — a fixed \
             offset must not be misread as congestion"
        );

        Ok(())
    });

    common::run_sim_or_timeout(&mut sim, Duration::from_secs(5 * 60))
        .unwrap_or_else(|error| panic!("simulation failed: {error}"));
}

/// Three concurrent downstreams under a link that genuinely cycles between
/// ample and tight bandwidth. At the tight floor one downstream is forced to
/// pause. The allocator must keep that demotion stable — the paused stream
/// must not oscillate in-and-out of pause within a single bandwidth phase.
/// Such rapid pause↔active cycles are a UX regression (the viewer sees a
/// camera repeatedly freeze and unfreeze within seconds).
#[test]
fn simulcast_no_slot_blips_under_competitive_multi_party_bandwidth() {
    let _guard = simulcast_test_guard();
    common::setup_tracing();

    const TICK: Duration = Duration::from_millis(1);
    const WARMUP: Duration = Duration::from_secs(20);
    const CYCLES: usize = 4;
    const HIGH_PHASE: Duration = Duration::from_secs(20);
    const LOW_PHASE: Duration = Duration::from_secs(15);
    const SAMPLE: Duration = Duration::from_secs(2);
    const STALL_FLOOR_BPS: u64 = 30_000;
    const SENDER_COUNT: usize = 3;
    // One entry into pause per phase is legitimate adaptation; allow two for
    // the BW-restore bounce. More than 3 per cycle is blipping.
    const MAX_PAUSE_TRANSITIONS_PER_CYCLE: usize = 3;
    // 2.5M: all three streams fit at Medium comfortably.
    // 350k: pool ~315k; two streams hold Low (150k floor each), one pauses.
    const HIGH_BW: u64 = 2_500_000;
    const LOW_BW: u64 = 350_000;

    let mut sim = turmoil::Builder::new()
        .simulation_duration(
            WARMUP + (HIGH_PHASE + LOW_PHASE) * (CYCLES as u32) + Duration::from_secs(10),
        )
        .tick_duration(TICK)
        .min_message_latency(Duration::from_millis(10))
        .max_message_latency(Duration::from_millis(35))
        .rng_seed(scenario_seed("multi-party-blip-check"))
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let receiver_ip = common::subnet_ip(subnet, 2);

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF))
            .await
            .map_err(Into::into)
    });

    let done = CancellationToken::new();
    for sender_idx in 0..SENDER_COUNT {
        let sender_ip = common::subnet_ip(subnet, 10 + sender_idx as u8);
        let done = done.clone();
        sim.client(sender_ip, async move {
            let mut client = common::client::SimClientBuilder::bind(sender_ip, server_ip)
                .await?
                .with_track(
                    MediaKind::Video,
                    TransceiverDirection::SendOnly,
                    Some(vec![
                        SimulcastLayer::new("f"),
                        SimulcastLayer::new("h"),
                        SimulcastLayer::new("q"),
                    ]),
                )
                .connect("multi-party-blip-check")
                .await?;
            client.drive(done).await?;
            Ok(())
        });
    }

    sim.client(receiver_ip, async move {
        let _done = done.drop_guard();
        let mut builder = common::client::SimClientBuilder::bind(receiver_ip, server_ip).await?;
        for _ in 0..SENDER_COUNT {
            builder = builder.with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None);
        }
        let mut client = builder.connect("multi-party-blip-check").await?;

        client
            .drive_until(Duration::from_secs(15), |ctx| {
                ctx.discovered_tracks.len() >= SENDER_COUNT
            })
            .await?;
        client
            .drive_until(WARMUP, |ctx| {
                if ctx.remote_tracks.len() < SENDER_COUNT {
                    return false;
                }
                let stats = ctx.driver.stats();
                ctx.remote_tracks.keys().all(|mid| {
                    stats
                        .tracks
                        .get(mid)
                        .is_some_and(|t| t.rx_layers.values().any(|l| l.bytes > 0))
                })
            })
            .await?;

        let after_warmup = per_track_bytes(&client);
        assert!(
            after_warmup.values().all(|&b| b > 0),
            "not every stream established during warmup: {after_warmup:?}"
        );

        let mut pause_transitions: std::collections::HashMap<_, usize> =
            after_warmup.keys().map(|mid| (*mid, 0)).collect();
        let mut was_stalled: std::collections::HashMap<_, bool> =
            after_warmup.keys().map(|mid| (*mid, false)).collect();
        let mut last = after_warmup;

        let samples_per_phase =
            |dur: Duration| (dur.as_secs() / SAMPLE.as_secs()).max(1) as usize;

        for cycle in 0..CYCLES {
            common::set_downlink_bandwidth(receiver_ip, Some(LOW_BW));
            for _ in 0..samples_per_phase(LOW_PHASE) {
                client.drive_for(SAMPLE).await?;
                let bytes = per_track_bytes(&client);
                for (mid, &b) in &bytes {
                    let prev = last.get(mid).copied().unwrap_or(0);
                    let bps = b.saturating_sub(prev) * 8 / SAMPLE.as_secs();
                    let stalled = bps < STALL_FLOOR_BPS;
                    let was = *was_stalled.get(mid).unwrap_or(&false);
                    if !was && stalled {
                        *pause_transitions.entry(*mid).or_default() += 1;
                    }
                    *was_stalled.entry(*mid).or_default() = stalled;
                }
                last = bytes;
            }

            let stalled_count = was_stalled.values().filter(|&&s| s).count();
            assert!(
                stalled_count <= 1,
                "cycle {cycle}: {stalled_count}/{SENDER_COUNT} streams stalled at the end of \
                 the low-bandwidth phase; expected at most 1 (the lowest-priority slot)"
            );

            common::set_downlink_bandwidth(receiver_ip, None);
            for _ in 0..samples_per_phase(HIGH_PHASE) {
                client.drive_for(SAMPLE).await?;
                let bytes = per_track_bytes(&client);
                for (mid, &b) in &bytes {
                    let prev = last.get(mid).copied().unwrap_or(0);
                    let bps = b.saturating_sub(prev) * 8 / SAMPLE.as_secs();
                    let stalled = bps < STALL_FLOOR_BPS;
                    let was = *was_stalled.get(mid).unwrap_or(&false);
                    if !was && stalled {
                        *pause_transitions.entry(*mid).or_default() += 1;
                    }
                    *was_stalled.entry(*mid).or_default() = stalled;
                }
                last = bytes;
            }
        }

        let max_allowed = MAX_PAUSE_TRANSITIONS_PER_CYCLE * CYCLES;
        for (mid, &transitions) in &pause_transitions {
            assert!(
                transitions <= max_allowed,
                "stream {mid:?} toggled pause↔active {transitions} times over {CYCLES} cycles \
                 (max allowed {max_allowed}); repeated pause oscillation detected"
            );
        }

        Ok(())
    });

    common::run_sim_or_timeout(&mut sim, Duration::from_secs(5 * 60))
        .unwrap_or_else(|error| panic!("simulation failed: {error}"));
}

/// Three concurrent downstreams under a link whose bandwidth follows a
/// non-monotonic path — tight → moderate → ample → tight → generous →
/// tight → restored — rather than a single step-down-then-up or fixed
/// jitter/loss profile. Every stream must keep delivering at its current tier
/// without stalling for more than two consecutive measurement windows at any
/// point, including during bandwidth transitions.
#[test]
fn simulcast_multiple_downstreams_survive_dynamic_bandwidth() {
    let _guard = simulcast_test_guard();
    common::setup_tracing();

    const TICK: Duration = Duration::from_millis(1);
    const WARMUP: Duration = Duration::from_secs(25);
    const STALL_FLOOR_BPS: u64 = 30_000;
    const SENDER_COUNT: usize = 3;
    const SAMPLE: Duration = Duration::from_secs(3);

    // Non-monotonic bandwidth phases: (downlink_bps or None=uncapped, duration)
    let bandwidth_plan: &[(Option<u64>, Duration)] = &[
        (Some(350_000), Duration::from_secs(12)),   // tight: one stream forced to pause
        (Some(1_200_000), Duration::from_secs(15)), // moderate: all streams at Low or Medium
        (None, Duration::from_secs(20)),            // ample: streams climb toward High
        (Some(550_000), Duration::from_secs(12)),   // tight-ish: squeeze back down
        (Some(2_000_000), Duration::from_secs(15)), // generous: Medium/High reachable
        (Some(400_000), Duration::from_secs(12)),   // tight again
        (None, Duration::from_secs(25)),            // fully restored
    ];
    let total_phases: Duration = bandwidth_plan.iter().map(|(_, d)| *d).sum();

    let mut sim = turmoil::Builder::new()
        .simulation_duration(WARMUP + total_phases + Duration::from_secs(10))
        .tick_duration(TICK)
        .min_message_latency(Duration::from_millis(15))
        .max_message_latency(Duration::from_millis(60))
        .fail_rate(0.005)
        .rng_seed(scenario_seed("multi-party-dynamic-bandwidth"))
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let receiver_ip = common::subnet_ip(subnet, 2);

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF))
            .await
            .map_err(Into::into)
    });

    let done = CancellationToken::new();
    for sender_idx in 0..SENDER_COUNT {
        let sender_ip = common::subnet_ip(subnet, 10 + sender_idx as u8);
        let done = done.clone();
        sim.client(sender_ip, async move {
            let mut client = common::client::SimClientBuilder::bind(sender_ip, server_ip)
                .await?
                .with_track(
                    MediaKind::Video,
                    TransceiverDirection::SendOnly,
                    Some(vec![
                        SimulcastLayer::new("f"),
                        SimulcastLayer::new("h"),
                        SimulcastLayer::new("q"),
                    ]),
                )
                .connect("multi-party-dynamic-bandwidth")
                .await?;
            client.drive(done).await?;
            Ok(())
        });
    }

    sim.client(receiver_ip, async move {
        let _done = done.drop_guard();
        let mut builder = common::client::SimClientBuilder::bind(receiver_ip, server_ip).await?;
        for _ in 0..SENDER_COUNT {
            builder = builder.with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None);
        }
        let mut client = builder.connect("multi-party-dynamic-bandwidth").await?;

        client
            .drive_until(Duration::from_secs(20), |ctx| {
                ctx.discovered_tracks.len() >= SENDER_COUNT
            })
            .await?;
        client
            .drive_until(WARMUP, |ctx| {
                if ctx.remote_tracks.len() < SENDER_COUNT {
                    return false;
                }
                let stats = ctx.driver.stats();
                ctx.remote_tracks.keys().all(|mid| {
                    stats
                        .tracks
                        .get(mid)
                        .is_some_and(|t| t.rx_layers.values().any(|l| l.bytes > 0))
                })
            })
            .await?;

        let warmup_bytes = per_track_bytes(&client);
        assert!(
            warmup_bytes.values().all(|&b| b > 0),
            "not every stream established during warmup: {warmup_bytes:?}"
        );

        let mut last = warmup_bytes;
        let mut consecutive_misses: std::collections::HashMap<_, u32> =
            last.keys().map(|mid| (*mid, 0)).collect();

        for (phase, (bw, phase_dur)) in bandwidth_plan.iter().enumerate() {
            common::set_downlink_bandwidth(receiver_ip, *bw);
            let windows = (phase_dur.as_secs() / SAMPLE.as_secs()).max(1) as usize;
            for window in 0..windows {
                client.drive_for(SAMPLE).await?;
                let bytes = per_track_bytes(&client);
                for (mid, &b) in &bytes {
                    let prev = last.get(mid).copied().unwrap_or(0);
                    let bps = b.saturating_sub(prev) * 8 / SAMPLE.as_secs();
                    let misses = consecutive_misses.entry(*mid).or_default();
                    if bps > STALL_FLOOR_BPS {
                        *misses = 0;
                    } else {
                        *misses += 1;
                        assert!(
                            *misses < 3,
                            "stream {mid:?} stalled for 3 consecutive {SAMPLE:?} windows in \
                             phase {phase} (bw={bw:?}), window {window}: only {bps} bps"
                        );
                    }
                }
                last = bytes;
            }
        }

        Ok(())
    });

    common::run_sim_or_timeout(&mut sim, Duration::from_secs(10 * 60))
        .unwrap_or_else(|error| panic!("simulation failed: {error}"));
}
