//! Group calls: several publishers competing for one viewer's downlink.
//! Real group calls are rarely spacious -- someone is always on a weaker
//! connection than the rest of the room -- so every scenario here runs
//! under genuine, sustained contention rather than ample bandwidth. The
//! defining property of a group call, on top of the single-stream
//! properties every other module checks, is *prioritization*: the viewer's
//! attention (active speaker, pinned participant) must be reflected in who
//! gets the pool first.

use super::support::{self};
use pulsebeam_agent::manager::Subscription;
use pulsebeam_agent::{MediaKind, TransceiverDirection};
use std::collections::HashMap;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// A 6-person call on a downlink that can't carry everyone at full quality
/// at once (typical: one participant on a decent-but-not-great connection).
/// The viewer has weighted the active speaker highest via
/// `Subscription::height` -- the same knob `VideoAllocator` turns directly
/// into `Slot::weight` for its water-filling allocation. A correct
/// allocator must let the active speaker's QoE clearly lead the room, while
/// every other participant still clears a baseline floor -- prioritization
/// is a *ranking*, not an excuse to starve everyone else.
#[test]
fn constrained_group_call_prioritizes_active_speaker() {
    let _guard = support::test_guard();
    const TICK: Duration = Duration::from_millis(1);
    const WARMUP_TIMEOUT: Duration = Duration::from_secs(30);
    const SUSTAIN: Duration = Duration::from_secs(60);
    const SENDER_COUNT: usize = 6;
    // Six participants' quarter-layer baseline alone is ~900kbit/s; this
    // cap leaves only modest climbing surplus, forcing the allocator to
    // make a real choice about who gets it rather than everyone drifting up
    // together.
    const CONSTRAINED_BPS: u64 = 1_800_000;

    let mut sim = turmoil::Builder::new()
        .simulation_duration(WARMUP_TIMEOUT + SUSTAIN + Duration::from_secs(10))
        .tick_duration(TICK)
        .min_message_latency(support::CONGESTED_WIFI.min_latency)
        .max_message_latency(support::CONGESTED_WIFI.max_latency)
        .fail_rate(support::CONGESTED_WIFI.fail_rate)
        .rng_seed(support::seed("multi_party_active_speaker"))
        .build();

    let subnet = crate::tests::common::reserve_subnet();
    let server_ip = crate::tests::common::subnet_ip(subnet, 1);
    let receiver_ip = crate::tests::common::subnet_ip(subnet, 2);

    support::spawn_sfu(&mut sim, server_ip);
    let done = CancellationToken::new();
    for i in 0..SENDER_COUNT {
        let sender_ip = crate::tests::common::subnet_ip(subnet, 10 + i as u8);
        support::spawn_publisher(
            &mut sim,
            sender_ip,
            server_ip,
            "multi-party-active-speaker",
            done.clone(),
        );
    }

    sim.client(receiver_ip, async move {
        let _done = done.drop_guard();
        let mut builder = crate::tests::common::client::SimClientBuilder::bind(receiver_ip, server_ip).await?;
        for _ in 0..SENDER_COUNT {
            builder = builder.with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None);
        }
        let mut client = builder.connect("multi-party-active-speaker").await?;

        support::warmup_until_all_flowing(&mut client, WARMUP_TIMEOUT, SENDER_COUNT).await?;

        let mut track_ids: Vec<String> = client.ctx.discovered_tracks.iter().cloned().collect();
        track_ids.sort();
        assert_eq!(track_ids.len(), SENDER_COUNT);

        // One active speaker (weight 1080), everyone else a small equally
        // weighted gallery tile (weight 180).
        let weights: Vec<u32> = std::iter::once(1080u32)
            .chain(std::iter::repeat(180u32).take(SENDER_COUNT - 1))
            .collect();
        let subscriptions: Vec<Subscription> = track_ids
            .iter()
            .zip(weights.iter())
            .map(|(track_id, height)| Subscription {
                track_id: track_id.clone(),
                height: *height,
            })
            .collect();
        let weight_by_track_id: HashMap<String, (&str, u32)> = track_ids
            .iter()
            .zip(weights.iter())
            .enumerate()
            .map(|(i, (track_id, height))| {
                let label = if i == 0 { "active_speaker" } else { "gallery" };
                (track_id.clone(), (label, *height))
            })
            .collect();
        client.ctx.driver.set_subscriptions(subscriptions);
        client.drive_for(Duration::from_secs(15)).await?;

        crate::tests::common::set_downlink_bandwidth(receiver_ip, Some(CONSTRAINED_BPS));
        client.drive_for(Duration::from_secs(10)).await?;
        client.ctx.mark_qoe_baseline();
        client.drive_for(SUSTAIN).await?;

        client.ctx.assert_all_streams_decodable();

        let scores = client.ctx.qoe_scores();
        let entries: Vec<(&str, f64, f64)> = client
            .ctx
            .remote_tracks
            .iter()
            .map(|(mid, track_id)| {
                let (label, weight) = weight_by_track_id
                    .get(track_id)
                    .copied()
                    .expect("every remote track was subscribed with a known weight");
                (label, weight as f64, scores.get(mid).copied().unwrap_or(0.0))
            })
            .collect();
        assert_eq!(entries.len(), SENDER_COUNT);
        tracing::info!(?entries, "per-participant QoE score by priority weight");

        crate::tests::common::qoe::assert_priority_ordering(&entries, 10.0);
        // Prioritization must not become starvation: even a small gallery
        // tile should stay recognizably alive.
        client.ctx.assert_min_qoe_score(30.0);

        let active_speaker_score = entries
            .iter()
            .find(|(label, ..)| *label == "active_speaker")
            .expect("active speaker entry present")
            .2;
        assert!(
            active_speaker_score >= 65.0,
            "the active speaker scored only {active_speaker_score:.1} under contention -- \
             should be clearly favored by the allocator: {entries:?}"
        );

        Ok(())
    });

    crate::tests::common::run_sim_or_timeout(&mut sim, Duration::from_secs(5 * 60))
        .unwrap_or_else(|error| panic!("simulation failed: {error}"));
}

/// The dynamic version of the scenario above: the room starts with everyone
/// weighted equally, then partway through the call someone starts talking
/// and the viewer's client re-weights that *already-active* slot higher --
/// exactly what a real active-speaker UI does continuously. This must be
/// just as healthy as picking the winner up front: re-prioritizing who's on
/// screen is an ordinary, frequent event in a real call, not a rare edge
/// case, and it must not cost the promoted stream a multi-second (or
/// permanent) outage while the SFU re-acquires it.
#[test]
fn active_speaker_handoff_mid_call_stays_healthy() {
    let _guard = support::test_guard();
    const TICK: Duration = Duration::from_millis(1);
    const WARMUP_TIMEOUT: Duration = Duration::from_secs(30);
    const SUSTAIN: Duration = Duration::from_secs(45);
    const SENDER_COUNT: usize = 3;
    const CONSTRAINED_BPS: u64 = 1_400_000;

    let mut sim = turmoil::Builder::new()
        .simulation_duration(WARMUP_TIMEOUT + SUSTAIN + Duration::from_secs(20))
        .tick_duration(TICK)
        .min_message_latency(support::CONGESTED_WIFI.min_latency)
        .max_message_latency(support::CONGESTED_WIFI.max_latency)
        .rng_seed(support::seed("multi_party_speaker_handoff"))
        .build();

    let subnet = crate::tests::common::reserve_subnet();
    let server_ip = crate::tests::common::subnet_ip(subnet, 1);
    let receiver_ip = crate::tests::common::subnet_ip(subnet, 2);

    support::spawn_sfu(&mut sim, server_ip);
    let done = CancellationToken::new();
    for i in 0..SENDER_COUNT {
        let sender_ip = crate::tests::common::subnet_ip(subnet, 10 + i as u8);
        support::spawn_publisher(
            &mut sim,
            sender_ip,
            server_ip,
            "multi-party-speaker-handoff",
            done.clone(),
        );
    }

    sim.client(receiver_ip, async move {
        let _done = done.drop_guard();
        let mut builder = crate::tests::common::client::SimClientBuilder::bind(receiver_ip, server_ip).await?;
        for _ in 0..SENDER_COUNT {
            builder = builder.with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None);
        }
        let mut client = builder.connect("multi-party-speaker-handoff").await?;

        support::warmup_until_all_flowing(&mut client, WARMUP_TIMEOUT, SENDER_COUNT).await?;
        crate::tests::common::set_downlink_bandwidth(receiver_ip, Some(CONSTRAINED_BPS));
        client.drive_for(Duration::from_secs(10)).await?;

        let mut track_ids: Vec<String> = client.ctx.discovered_tracks.iter().cloned().collect();
        track_ids.sort();

        // Someone starts talking: promote the second participant from
        // gallery weight to active-speaker weight on an already-flowing
        // slot, exactly the re-stage-an-active-layer path.
        let promoted_track = track_ids[1].clone();
        let subscriptions: Vec<Subscription> = track_ids
            .iter()
            .map(|track_id| Subscription {
                track_id: track_id.clone(),
                height: if *track_id == promoted_track { 1080 } else { 180 },
            })
            .collect();
        client.ctx.driver.set_subscriptions(subscriptions);

        // The property under test: recovery is bounded, not "eventually".
        client.ctx.mark_qoe_baseline();
        client.drive_for(Duration::from_secs(3)).await?;
        client
            .ctx
            .assert_max_freeze_under(Duration::from_secs(3));

        client.drive_for(SUSTAIN).await?;
        client.ctx.assert_all_streams_decodable();
        client.ctx.assert_min_qoe_score(55.0);

        Ok(())
    });

    crate::tests::common::run_sim_or_timeout(&mut sim, Duration::from_secs(5 * 60))
        .unwrap_or_else(|error| panic!("simulation failed: {error}"));
}

/// A larger, loosely-moderated call (e.g. a class or a big team standup)
/// where nobody has been explicitly prioritized -- everyone shares the pool
/// under the allocator's default weighting. The property here is fairness:
/// the pool is tight enough that not everyone can be at full quality, but
/// no participant should be *permanently* starved while others thrive.
#[test]
fn large_group_call_shares_bandwidth_fairly() {
    let _guard = support::test_guard();
    const TICK: Duration = Duration::from_millis(1);
    const WARMUP_TIMEOUT: Duration = Duration::from_secs(40);
    const SUSTAIN_WINDOWS: usize = 12;
    const SUSTAIN_WINDOW: Duration = Duration::from_secs(8);
    const SENDER_COUNT: usize = 9;
    const STALL_FLOOR_BPS: u64 = 25_000;
    const CONSTRAINED_BPS: u64 = 2_200_000;

    let mut sim = turmoil::Builder::new()
        .simulation_duration(
            WARMUP_TIMEOUT + SUSTAIN_WINDOW * (SUSTAIN_WINDOWS as u32) + Duration::from_secs(10),
        )
        .tick_duration(TICK)
        .min_message_latency(support::CONGESTED_WIFI.min_latency)
        .max_message_latency(support::CONGESTED_WIFI.max_latency)
        .rng_seed(support::seed("multi_party_large_group_fairness"))
        .build();

    let subnet = crate::tests::common::reserve_subnet();
    let server_ip = crate::tests::common::subnet_ip(subnet, 1);
    let receiver_ip = crate::tests::common::subnet_ip(subnet, 2);

    support::spawn_sfu(&mut sim, server_ip);
    let done = CancellationToken::new();
    for i in 0..SENDER_COUNT {
        let sender_ip = crate::tests::common::subnet_ip(subnet, 10 + i as u8);
        support::spawn_publisher(
            &mut sim,
            sender_ip,
            server_ip,
            "multi-party-large-group",
            done.clone(),
        );
    }

    sim.client(receiver_ip, async move {
        let _done = done.drop_guard();
        let mut builder = crate::tests::common::client::SimClientBuilder::bind(receiver_ip, server_ip).await?;
        for _ in 0..SENDER_COUNT {
            builder = builder.with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None);
        }
        let mut client = builder.connect("multi-party-large-group").await?;

        support::warmup_until_all_flowing(&mut client, WARMUP_TIMEOUT, SENDER_COUNT).await?;
        crate::tests::common::set_downlink_bandwidth(receiver_ip, Some(CONSTRAINED_BPS));
        client.drive_for(Duration::from_secs(10)).await?;

        client.ctx.assert_all_streams_decodable();
        let mut last = support::per_track_bytes(&client);
        let mut ever_delivered: HashMap<_, bool> = last.keys().map(|mid| (*mid, false)).collect();

        for window in 0..SUSTAIN_WINDOWS {
            client.drive_for(SUSTAIN_WINDOW).await?;
            let bytes = support::per_track_bytes(&client);
            for (mid, &b) in &bytes {
                let prev = last.get(mid).copied().unwrap_or(0);
                let bps = b.saturating_sub(prev) * 8 / SUSTAIN_WINDOW.as_secs();
                if bps > STALL_FLOOR_BPS {
                    *ever_delivered.entry(*mid).or_default() = true;
                }
            }
            tracing::debug!(window, "large group fairness sample taken");
            last = bytes;
        }

        client.ctx.assert_all_streams_decodable();
        let starved: Vec<_> = ever_delivered
            .iter()
            .filter(|entry| !*entry.1)
            .map(|entry| *entry.0)
            .collect();
        assert!(
            starved.is_empty(),
            "participant(s) {starved:?} never cleared the stall floor in any of the \
             {SUSTAIN_WINDOWS} windows -- a large call must rotate/share, not permanently \
             blackout a subset of participants"
        );

        Ok(())
    });

    crate::tests::common::run_sim_or_timeout(&mut sim, Duration::from_secs(5 * 60))
        .unwrap_or_else(|error| panic!("simulation failed: {error}"));
}
