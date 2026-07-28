use std::time::Duration;

use pulsebeam_agent::manager::Subscription;
use pulsebeam_agent::{MediaKind, SimulcastLayer, TransceiverDirection};
use tokio_util::sync::CancellationToken;

use crate::tests::common::{self, client::SimClientBuilder};

/// Forces the SFU to switch a subscriber between simulcast layers over and over
/// and asserts the subscriber can still decode what comes out.
///
/// The decode-side verdict comes from str0m's own depacketizer on the receiving
/// client: `contiguous` is false whenever a frame followed a sequence-number
/// hole, and `is_keyframe` marks a decodable entry point. A switch that drops
/// parameter sets, reuses a timestamp, or renumbers around missing packets shows
/// up here and nowhere in a bytes-received counter.
#[test]

fn repeated_simulcast_switching_stays_decodable_test() {
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(120))
        .tick_duration(Duration::from_millis(1))
        .rng_seed(0xDEADBEEF)
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let publisher_ip = common::subnet_ip(subnet, 2);
    let subscriber_ip = common::subnet_ip(subnet, 3);

    let done = CancellationToken::new();

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF))
            .await
            .map_err(Into::into)
    });

    let publisher_done = done.clone();
    sim.client(publisher_ip, async move {
        let mut client = SimClientBuilder::bind(publisher_ip, server_ip)
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
        client.drive(publisher_done).await.ok();
        Ok(())
    });

    let subscriber_done = done.clone();
    sim.client(subscriber_ip, async move {
        let mut client = SimClientBuilder::bind(subscriber_ip, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .connect("room1")
            .await?;

        client
            .drive_until(Duration::from_secs(15), |ctx| {
                !ctx.discovered_tracks.is_empty()
            })
            .await?;
        let track_id = client
            .ctx
            .discovered_tracks
            .iter()
            .next()
            .expect("a published track")
            .clone();

        let subscribe_at = |height: u32| Subscription {
            track_id: track_id.clone(),
            height,
            min_height: 0,
            priority: 0,
        };

        // Establish the highest layer and let it settle.
        client.ctx.driver.set_subscriptions(vec![subscribe_at(720)]);
        let log = client.ctx.video_rx.clone();
        client
            .drive_until(Duration::from_secs(20), move |_| {
                log.lock().unwrap().frames > 30
            })
            .await?;
        let _ = client.drive_for(Duration::from_secs(3)).await;

        let baseline = client.ctx.video_rx.lock().unwrap().clone();
        let sample = |c: &mut crate::tests::common::client::SimClient| {
            let stats = c.ctx.driver.stats();
            let nacks: u64 = stats
                .tracks
                .values()
                .flat_map(|t| t.rx_layers.values())
                .map(|l| l.nacks)
                .sum();
            let bwe = stats
                .peer
                .as_ref()
                .and_then(|p| p.bwe_tx)
                .map(|b| b.as_f64());
            (nacks, bwe)
        };
        let (nacks_before, bwe_before) = sample(&mut client);

        // Now flip between layers repeatedly. Each flip forces a full switch:
        // new SSRC upstream, new resolution, new parameter sets.
        // Strictly alternating heights so every step is a genuine layer change.
        const SWITCHES: usize = 12;
        for i in 0..SWITCHES {
            let height = if i % 2 == 0 { 180 } else { 720 };
            client
                .ctx
                .driver
                .set_subscriptions(vec![subscribe_at(height)]);
            let _ = client.drive_for(Duration::from_millis(2500)).await;
        }

        let _ = client.drive_for(Duration::from_secs(3)).await;

        let (nacks_after, bwe_after) = sample(&mut client);
        let final_log = client.ctx.video_rx.lock().unwrap().clone();
        let frames = final_log.frames - baseline.frames;
        let keyframes = final_log.keyframes - baseline.keyframes;
        let broken = final_log.non_contiguous - baseline.non_contiguous;
        let duplicates = final_log.duplicate_ts_frames - baseline.duplicate_ts_frames;
        let nacks = nacks_after;

        assert!(
            frames > 200,
            "expected the stream to keep flowing across switches, got {frames} frames"
        );
        // A subscription change can be superseded by the next one before its
        // keyframe arrives, so this does not reach SWITCHES exactly.
        assert!(
            keyframes >= SWITCHES as u64 * 2 / 3,
            "only {keyframes} decodable keyframes arrived across {SWITCHES} switches; \
             switching is not completing"
        );
        let undecodable =
            final_log.keyframes_missing_parameter_sets - baseline.keyframes_missing_parameter_sets;
        assert_eq!(
            undecodable, 0,
            "{undecodable} of {keyframes} keyframes reached the decoder without the \
             SPS/PPS describing them — the picture would be garbage after those switches"
        );
        // The SFU never reuses a timestamp — `rtp::egress_guard` asserts that on
        // the write path itself. What reaches the decoder twice is str0m
        // re-emitting a frame whose retransmission landed late, so this is
        // bounded by retransmissions rather than zero.
        assert!(
            duplicates <= nacks,
            "{duplicates} frames arrived with a reused timestamp but only {nacks} \
             packets were retransmitted — the forwarder is duplicating media"
        );
        // Ordinary UDP reordering moves this by a frame or two; a switch that
        // rewinds the output clock moves it by far more.
        let max_regression = final_log.max_ts_regression;
        assert!(
            max_regression < 30_000,
            "output RTP clock jumped backwards by {max_regression} ticks \
             ({:.0}ms) — far beyond network reordering",
            max_regression as f64 / 90.0
        );

        // Switching truncates the old layer's in-flight frame, so at most one
        // broken frame per switch is inherent. Anything beyond that is corruption.
        let allowed = SWITCHES as u64;
        assert!(
            broken <= allowed,
            "{broken} frames arrived with a sequence hole over {SWITCHES} switches \
             (at most {allowed} expected); frames={frames} keyframes={keyframes}"
        );

        // Every sequence number the SFU skips is one the subscriber NACKs for and
        // then reports as loss. str0m's retransmission cache never held those
        // packets, so the NACKs can never be answered and the loss walks the
        // bandwidth estimate down — the switch degrades the stream it just
        // switched to.
        let switch_nacks = nacks_after - nacks_before;
        tracing::info!(
            frames,
            keyframes,
            broken,
            duplicates,
            switch_nacks,
            ?bwe_before,
            ?bwe_after,
            "switch soak result"
        );
        assert!(
            switch_nacks <= frames,
            "{switch_nacks} retransmission requests over {SWITCHES} switches \
             ({frames} frames) — switching is manufacturing loss"
        );

        if let (Some(before), Some(after)) = (bwe_before, bwe_after) {
            assert!(
                after >= before * 0.6,
                "bandwidth estimate fell from {before:.0} to {after:.0} across \
                 {SWITCHES} switches; the switch bursts are being read as congestion"
            );
        }

        subscriber_done.cancel();
        Ok(())
    });

    common::run_sim_or_timeout(&mut sim, Duration::from_secs(300)).expect("simulation");
}
