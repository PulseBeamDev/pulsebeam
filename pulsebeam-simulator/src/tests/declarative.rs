use std::time::Duration;
use crate::tests::common::{setup_tracing, start_sfu_node, client::SimClientBuilder};
use pulsebeam_agent::manager::Subscription;
use pulsebeam_agent::TransceiverDirection;
use pulsebeam_agent::MediaKind;
use std::net::IpAddr;

#[test]
fn declarative_subscription_test() -> turmoil::Result {
    setup_tracing();
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(30))
        .build();

    sim.host("sfu", move || async move {
        let sfu_ip = turmoil::lookup("sfu");
        crate::tests::common::start_sfu_node(sfu_ip).await.map_err(|e| e.into())
    });

    sim.client("client1", async move {
        let sfu_ip = turmoil::lookup("sfu");
        let client1_ip = turmoil::lookup("client1");
        let mut client = SimClientBuilder::bind(client1_ip, sfu_ip).await?
            .with_track(MediaKind::Video, TransceiverDirection::SendOnly, None)
            .connect("room1").await?;
        
        client.drive_until(Duration::from_secs(10), |_| false).await.ok();

        Ok(())
    });

    sim.client("client2", async move {
        let sfu_ip = turmoil::lookup("sfu");
        let client2_ip = turmoil::lookup("client2");
        let mut client = SimClientBuilder::bind(client2_ip, sfu_ip).await?
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .connect("room1").await?;
        
        // 1. Set declarative subscriptions
        client.agent.set_subscriptions(vec![
            Subscription { track_id: "client1_video".to_string(), height: 720 },
        ]).await?;

        // 2. Wait for media flow
        tracing::info!("Waiting for media flow via declarative subscription...");
        client.drive_until(Duration::from_secs(10), |stats| {
            stats.total_rx_bytes() > 20_000
        }).await?;

        tracing::info!("Media flow established!");

        // 3. Update subscriptions (Sticky test)
        // Add a non-existent track, Alice should stay on the same MID (internal check via logs)
        client.agent.set_subscriptions(vec![
            Subscription { track_id: "client1_video".to_string(), height: 360 },
            Subscription { track_id: "non_existent".to_string(), height: 360 },
        ]).await?;

        client.drive_until(Duration::from_secs(5), |_| false).await.ok();
        
        Ok(())
    });

    sim.run()
}

#[test]
fn reconnection_recovery_test() -> turmoil::Result {
    setup_tracing();
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(60))
        .build();

    sim.host("sfu", move || async move {
        let sfu_ip = turmoil::lookup("sfu");
        crate::tests::common::start_sfu_node(sfu_ip).await.map_err(|e| e.into())
    });

    sim.client("client1", async move {
        let sfu_ip = turmoil::lookup("sfu");
        let client1_ip = turmoil::lookup("client1");
        let mut client = SimClientBuilder::bind(client1_ip, sfu_ip).await?
            .with_track(MediaKind::Video, TransceiverDirection::SendOnly, None)
            .connect("room1").await?;
        
        // 1. Establish initial flow
        client.drive_until(Duration::from_secs(10), |stats| {
            stats.total_tx_bytes() > 20_000
        }).await?;
        tracing::info!("Initial flow established");

        // 2. Simulate network failure (partition)
        tracing::info!("Simulating network partition...");
        turmoil::partition("client1", "sfu");

        // Drive for a bit while partitioned
        client.drive_until(Duration::from_secs(10), |_| false).await.ok();

        // 3. Lift partition and verify recovery
        tracing::info!("Lifting network partition...");
        turmoil::repair("client1", "sfu");

        // Wait for agent to reconnect and resume flow
        tracing::info!("Waiting for reconnection and flow recovery...");
        let start_bytes = client.agent.get_stats().await.map(|s| s.total_tx_bytes()).unwrap_or(0);
        client.drive_until(Duration::from_secs(20), |stats| {
            stats.total_tx_bytes() > start_bytes + 20_000
        }).await?;

        tracing::info!("Flow recovered after reconnection!");

        Ok(())
    });

    sim.run()
}
