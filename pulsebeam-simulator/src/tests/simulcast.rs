use super::common;
use pulsebeam_agent::{MediaKind, SimulcastLayer, TransceiverDirection};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[test]
fn simulcast_stream_stability_test() {
    common::setup_tracing();

    const TICK: Duration = Duration::from_millis(1);
    const WARMUP: Duration = Duration::from_secs(5);
    const SOAK: Duration = Duration::from_secs(55);
    const HEALTH_INTERVAL: Duration = Duration::from_secs(5);
    const MIN_BITS_PER_INTERVAL: u64 = 10_000; // must make forward progress each window

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
        let stats = client.agent.get_stats().await.unwrap();
        assert!(stats.peer.is_some(), "stream did not establish within warmup window");

        tracing::info!("stream established, entering soak...");

        // Phase 2: soak — assert stream is still making progress every interval
        let num_intervals = SOAK.as_secs() / HEALTH_INTERVAL.as_secs();
        let mut last_bits_rx: u64 = 0;

        for i in 0..num_intervals {
                client.drive_for(HEALTH_INTERVAL).await.unwrap();
            let stats = client.agent.get_stats().await.unwrap();
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
