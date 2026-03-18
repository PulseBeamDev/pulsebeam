use crate::tests::common::{self, client::SimClientBuilder, run_sim_or_timeout, setup_tracing};
use pulsebeam_agent::MediaKind;
use pulsebeam_agent::TransceiverDirection;
use pulsebeam_agent::manager::Subscription;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[test]
fn declarative_subscription_test() -> turmoil::Result {
    setup_tracing();
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(30))
        .build();

    let subnet = common::reserve_subnet();
    let sfu_ip = common::subnet_ip(subnet, 1);

    sim.host(sfu_ip, move || async move {
        crate::tests::common::start_sfu_node(
            sfu_ip,
            pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF),
        )
        .await
        .map_err(|e| e.into())
    });

    let done = CancellationToken::new();
    let client1_ip = common::subnet_ip(subnet, 2);
    let client2_ip = common::subnet_ip(subnet, 3);

    sim.client(client1_ip, {
        let done = done.clone();
        async move {
            let sfu_ip = sfu_ip;
            let client1_ip = client1_ip;
            let mut client = SimClientBuilder::bind(client1_ip, sfu_ip)
                .await?
                .with_track(MediaKind::Video, TransceiverDirection::SendOnly, None)
                .connect("room1")
                .await?;

            client.drive(done).await.ok();

            Ok(())
        }
    });

    sim.client(client2_ip, async move {
        let _done = done.drop_guard();
        let sfu_ip = sfu_ip;
        let client2_ip = client2_ip;
        let mut client = SimClientBuilder::bind(client2_ip, sfu_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .connect("room1")
            .await?;

        // 1. Wait for the publisher to advertise its track via signaling discovery.
        let start = tokio::time::Instant::now();
        while start.elapsed() < Duration::from_secs(30) {
            if !client.discovered_tracks.is_empty() {
                break;
            }
            client
                .drive_until(Duration::from_millis(100), |_| false)
                .await
                .ok();
        }

        let track_id = client
            .discovered_tracks
            .first()
            .cloned()
            .expect("No remote tracks discovered");

        // 2. Set declarative subscriptions
        client
            .agent
            .set_subscriptions(vec![Subscription {
                track_id: track_id.clone(),
                height: 720,
            }])
            .await?;

        // 2. Wait for media flow
        tracing::info!("Waiting for media flow via declarative subscription...");
        client
            .drive_until(Duration::from_secs(20), |stats| {
                // We only need a small amount of flow to consider the subscription active.
                stats.total_rx_bytes() > 1_000
            })
            .await?;

        tracing::info!("Media flow established!");

        // 3. Update subscriptions (Sticky test)
        // Add a non-existent track, Alice should stay on the same MID (internal check via logs)
        client
            .agent
            .set_subscriptions(vec![
                Subscription {
                    track_id: track_id.clone(),
                    height: 360,
                },
                Subscription {
                    track_id: "non_existent".to_string(),
                    height: 360,
                },
            ])
            .await?;

        client
            .drive_until(Duration::from_secs(5), |_| false)
            .await
            .ok();

        Ok(())
    });

    run_sim_or_timeout(&mut sim, Duration::from_secs(30))?;
    Ok(())
}

#[test]
fn reconnection_recovery_test() -> turmoil::Result {
    setup_tracing();
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(60))
        .build();

    let subnet = common::reserve_subnet();
    let sfu_ip = common::subnet_ip(subnet, 1);
    let client1_ip = common::subnet_ip(subnet, 2);

    sim.host(sfu_ip, move || async move {
        crate::tests::common::start_sfu_node(
            sfu_ip,
            pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF),
        )
        .await
        .map_err(|e| e.into())
    });

    sim.client(client1_ip, async move {
        let sfu_ip = sfu_ip;
        let client1_ip = client1_ip;
        let mut client = SimClientBuilder::bind(client1_ip, sfu_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::SendOnly, None)
            .connect("room1")
            .await?;

        // 1. Establish initial flow
        client
            .drive_until(Duration::from_secs(10), |stats| {
                stats.total_tx_bytes() > 20_000
            })
            .await?;
        tracing::info!("Initial flow established");

        // 2. Simulate network failure (partition)
        tracing::info!("Simulating network partition...");
        turmoil::partition(client1_ip, sfu_ip);

        // Drive for a bit while partitioned
        client
            .drive_until(Duration::from_secs(10), |_| false)
            .await
            .ok();

        // 3. Lift partition and verify recovery
        tracing::info!("Lifting network partition...");
        turmoil::repair(client1_ip, sfu_ip);

        // Wait for agent to reconnect and resume flow
        tracing::info!("Waiting for reconnection and flow recovery...");
        let start_bytes = client
            .agent
            .get_stats()
            .await
            .map(|s| s.total_tx_bytes())
            .unwrap_or(0);
        client
            .drive_until(Duration::from_secs(20), |stats| {
                stats.total_tx_bytes() > start_bytes + 20_000
            })
            .await?;

        tracing::info!("Flow recovered after reconnection!");

        Ok(())
    });

    run_sim_or_timeout(&mut sim, Duration::from_secs(30))?;
    Ok(())
}
