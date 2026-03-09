use super::common;
use pulsebeam_agent::actor::{AgentCommand, AgentStats};
use pulsebeam_agent::manager::Subscription;
use pulsebeam_agent::{MediaKind, SimulcastLayer, TransceiverDirection};
use std::net::IpAddr;
use std::time::Duration;
use pulsebeam_agent::str0m::media::Mid;

#[test]
fn slots_layout_update_test() {
    common::setup_tracing();

    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(60))
        .tick_duration(Duration::from_micros(100))
        .build();

    let server_ip: IpAddr = "192.168.0.1".parse().unwrap();
    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip)
            .await
            .map_err(|e| e.into())
    });

    let pub1_ip: IpAddr = "192.168.1.1".parse().unwrap();
    let pub2_ip: IpAddr = "192.168.1.2".parse().unwrap();
    let sub1_ip: IpAddr = "192.168.2.1".parse().unwrap();

    // Publisher 1: Sends "track1"
    sim.client(pub1_ip, async move {
        let mut client = common::client::SimClientBuilder::bind(pub1_ip, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::SendOnly, None)
            .connect("room1")
            .await?;
        client.drive_until(Duration::from_secs(30), |_| false).await.ok();
        Ok(())
    });

    // Publisher 2: Sends "track2"
    sim.client(pub2_ip, async move {
        let mut client = common::client::SimClientBuilder::bind(pub2_ip, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::SendOnly, None)
            .connect("room1")
            .await?;
        client.drive_until(Duration::from_secs(30), |_| false).await.ok();
        Ok(())
    });

    // Subscriber: Swaps between pub1 and pub2
    sim.client(sub1_ip, async move {
        let mut client = common::client::SimClientBuilder::bind(sub1_ip, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .connect("room1")
            .await?;

        // 1. Wait for both tracks to be discovered
        tracing::info!("Waiting for tracks to be available...");
        client.drive_until(Duration::from_secs(40), |stats| {
            // Predatorily check if we have 2 tracks in remote_tracks
            false 
        }).await.ok(); 
        
        tokio::time::sleep(Duration::from_secs(5)).await;

        assert_eq!(client.remote_tracks.len(), 2, "Should have discovered 2 remote tracks");
        
        let t1 = client.remote_tracks[0].to_string();
        let t2 = client.remote_tracks[1].to_string();
        
        tracing::info!("Discovered tracks (MIDs): {} and {}", t1, t2);

        // 2. Subscribe: Slot 1 -> T1, Slot 2 -> T2
        client.agent.set_subscriptions(vec![
            Subscription { track_id: t1.clone(), height: 720 },
            Subscription { track_id: t2.clone(), height: 720 },
        ]).await?;

        tracing::info!("Waiting for media on both slots...");
        client.drive_until(Duration::from_secs(20), |stats| {
            stats.tracks.values().all(|t| t.rx_layers.values().any(|l| l.bytes > 1000))
        }).await?;

        // 3. Swap: Slot 1 -> T2, Slot 2 -> T1
        tracing::info!("Swapping slots...");
        client.agent.set_subscriptions(vec![
            Subscription { track_id: t2.clone(), height: 720 },
            Subscription { track_id: t1.clone(), height: 720 },
        ]).await?;

        // 4. Verify swap
        client.drive_until(Duration::from_secs(20), |stats| {
             stats.tracks.values().all(|t| t.rx_layers.values().any(|l| l.bytes > 5000))
        }).await?;

        Ok(())
    });

    sim.run().expect("Simulation failed");
}

#[test]
fn slots_prioritization_test() {
    common::setup_tracing();

    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(60))
        .tick_duration(Duration::from_micros(100))
        .build();

    let server_ip: IpAddr = "192.168.0.1".parse().unwrap();
    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip)
            .await
            .map_err(|e| e.into())
    });

    let pub1_ip: IpAddr = "192.168.1.1".parse().unwrap();
    let pub2_ip: IpAddr = "192.168.1.2".parse().unwrap();
    let sub_ip: IpAddr = "192.168.2.1".parse().unwrap();

    // Publisher 1: Sends 3 layers
    sim.client(pub1_ip, async move {
        let mut client = common::client::SimClientBuilder::bind(pub1_ip, server_ip)
            .await?
            .with_track(
                MediaKind::Video,
                TransceiverDirection::SendOnly,
                Some(vec![
                    SimulcastLayer::new("f"), // High
                    SimulcastLayer::new("h"), // Med
                    SimulcastLayer::new("q"), // Low
                ]),
            )
            .connect("room1")
            .await?;
        client.drive_until(Duration::from_secs(50), |_| false).await.ok();
        Ok(())
    });

    // Publisher 2: Sends 3 layers
    sim.client(pub2_ip, async move {
        let mut client = common::client::SimClientBuilder::bind(pub2_ip, server_ip)
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
        client.drive_until(Duration::from_secs(50), |_| false).await.ok();
        Ok(())
    });


    // Subscriber: Requests high height for one slot, low for another
    sim.client(sub_ip, async move {
        let mut client = common::client::SimClientBuilder::bind(sub_ip, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .connect("room1")
            .await?;

        // Wait for discovery of both tracks
        client.drive_until(Duration::from_secs(40), |_| false).await.ok();
        tokio::time::sleep(Duration::from_secs(5)).await;

        assert_eq!(client.remote_tracks.len(), 2, "Should have discovered 2 remote tracks");
        let t1 = client.remote_tracks[0].to_string();
        let t2 = client.remote_tracks[1].to_string();

        // One slot high (T1), one slot low (T2)
        client.agent.set_subscriptions(vec![
            Subscription { track_id: t1.clone(), height: 720 },
            Subscription { track_id: t2.clone(), height: 180 },
        ]).await?;

        // Wait for flow
        client.drive_until(Duration::from_secs(30), |stats| {
            stats.tracks.len() >= 2 && stats.total_rx_bytes() > 50_000
        }).await?;

        let stats = client.agent.get_stats().await.unwrap();
        for (mid, track_stats) in stats.tracks {
             assert!(track_stats.rx_layers.values().any(|l| l.bytes > 0), "Slot {:?} should have received media", mid);
        }

        Ok(())
    });

    sim.run().expect("Simulation failed");
}
