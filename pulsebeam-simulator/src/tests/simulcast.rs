use super::common;
use pulsebeam_agent::{MediaKind, SimulcastLayer, TransceiverDirection};
use std::net::IpAddr;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[test]
fn simulcast_stream_stability_test() {
    common::setup_tracing();

    const TICK: Duration = Duration::from_millis(1);
    const WARMUP: Duration = Duration::from_secs(5);
    const SOAK: Duration = Duration::from_secs(55);
    const HEALTH_INTERVAL: Duration = Duration::from_secs(5);
    const MIN_BITS_PER_INTERVAL: u64 = 50_000; // must make forward progress each window

    let mut sim = turmoil::Builder::new()
        .simulation_duration(WARMUP + SOAK + Duration::from_secs(5)) // headroom
        .tick_duration(TICK)
        .build();

    let server_ip: IpAddr = "192.168.0.1".parse().unwrap();
    let sender_ip: IpAddr = "192.168.1.1".parse().unwrap();
    let receiver_ip: IpAddr = "192.168.2.1".parse().unwrap();

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip).await.map_err(Into::into)
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
        client
            .drive_until(WARMUP, |stats| {
                stats.peer.as_ref().is_none_or(|p| p.bytes_rx > 10_000)
            })
            .await
            .expect("stream did not establish within warmup window");

        tracing::info!("stream established, entering soak...");

        // Phase 2: soak — assert stream is still making progress every interval
        let num_intervals = SOAK.as_secs() / HEALTH_INTERVAL.as_secs();
        let mut last_bits_rx: u64 = 0;

        for i in 0..num_intervals {
            client
                .drive_until(HEALTH_INTERVAL, |_| false)
                .await
                .unwrap();

            let stats = client.agent.get_stats().await.unwrap();
            let bits_rx = stats.peer.as_ref().map_or(0, |p| p.bytes_rx) * 8;
            let delta = bits_rx.saturating_sub(last_bits_rx);

            tracing::info!(interval = i, bits_rx, delta, "health check");

            assert!(
                delta >= MIN_BITS_PER_INTERVAL,
                "stream stalled at interval {i}: only {delta} bits received \
                 in the last {HEALTH_INTERVAL:?} (total: {bits_rx})"
            );

            last_bits_rx = bits_rx;
        }

        tracing::info!("soak complete, stream remained stable");
        Ok(())
    });

    sim.run().expect("simulation failed");
}
