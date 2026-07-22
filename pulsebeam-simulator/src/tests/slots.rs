use super::common;
use pulsebeam_agent::manager::Subscription;
use pulsebeam_agent::{MediaKind, SimulcastLayer, TransceiverDirection};
use std::net::IpAddr;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[test]
fn slots_layout_update_test() -> turmoil::Result {
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(60))
        .tick_duration(Duration::from_micros(100))
        .rng_seed(0xDEADBEEF)
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let pub1_ip = common::subnet_ip(subnet, 2);
    let pub2_ip = common::subnet_ip(subnet, 3);
    let sub1_ip = common::subnet_ip(subnet, 4);

    // 1. Start SFU Node
    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF))
            .await
            .map_err(|e| e.into())
    });

    // 2. Publisher 1: Immediately publishes and streams
    sim.client(pub1_ip, async move {
        let mut client = common::client::SimClientBuilder::bind(pub1_ip, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::SendOnly, None)
            .connect("room1")
            .await?;

        client.drive_for(Duration::from_secs(50)).await.ok();
        Ok(())
    });

    // 3. Publisher 2: Immediately publishes and streams
    sim.client(pub2_ip, async move {
        let mut client = common::client::SimClientBuilder::bind(pub2_ip, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::SendOnly, None)
            .connect("room1")
            .await?;

        client.drive_for(Duration::from_secs(50)).await.ok();
        Ok(())
    });

    // 4. Subscriber: Discovers tracks over signaling and interacts with slots
    sim.client(sub1_ip, async move {
        let mut client = common::client::SimClientBuilder::bind(sub1_ip, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .connect("room1")
            .await?;

        // Replaced wait_for_remote_tracks: Drive until SFU signaling discovers both remote tracks
        client
            .drive_until(Duration::from_secs(10), |ctx| {
                ctx.discovered_tracks.len() >= 2
            })
            .await?;

        let track_ids: Vec<String> = client.ctx.discovered_tracks.iter().cloned().collect();
        if track_ids.len() < 2 {
            return Err("Expected at least 2 discovered tracks".into());
        }
        let track1 = &track_ids[0];
        let track2 = &track_ids[1];

        tracing::info!("Discovered remote track IDs: {} and {}", track1, track2);

        // Initial Layout Subscription: Slot 1 -> Track 1, Slot 2 -> Track 2
        client.ctx.driver.set_subscriptions(vec![
            Subscription {
                track_id: track1.clone(),
                height: 720,
            },
            Subscription {
                track_id: track2.clone(),
                height: 720,
            },
        ]);

        // Drive until media bytes start hitting rx_layers on both tracks
        tracing::info!("Waiting for initial media on both slots...");
        client
            .drive_until(Duration::from_secs(10), |ctx| {
                ctx.driver
                    .stats()
                    .tracks
                    .values()
                    .all(|t| t.rx_layers.values().any(|l| l.bytes > 0))
            })
            .await?;

        // Swap Layout: Slot 1 -> Track 2, Slot 2 -> Track 1
        tracing::info!("Swapping slots...");
        client.ctx.driver.set_subscriptions(vec![
            Subscription {
                track_id: track2.clone(),
                height: 720,
            },
            Subscription {
                track_id: track1.clone(),
                height: 720,
            },
        ]);

        // Verify media continues flowing after the swap step
        client
            .drive_until(Duration::from_secs(10), |ctx| {
                ctx.driver
                    .stats()
                    .tracks
                    .values()
                    .all(|t| t.rx_layers.values().any(|l| l.bytes > 1000))
            })
            .await?;

        Ok(())
    });

    common::run_sim_or_timeout(&mut sim, Duration::from_secs(30))?;
    Ok(())
}

#[test]
fn slots_prioritization_test() -> turmoil::Result {
    let mut sim = turmoil::Builder::new()
        .tick_duration(Duration::from_micros(100))
        .rng_seed(0)
        .build();

    let server_ip: IpAddr = "192.168.0.1".parse().unwrap();
    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF))
            .await
            .map_err(|e| e.into())
    });

    let pub1_ip: IpAddr = "192.168.1.1".parse().unwrap();
    let pub2_ip: IpAddr = "192.168.1.2".parse().unwrap();
    let sub_ip: IpAddr = "192.168.2.1".parse().unwrap();
    let token = CancellationToken::new();

    // Publisher 1: Sends 3 layers
    {
        let token = token.child_token();
        sim.client(pub1_ip, async move {
            let mut client = common::client::SimClientBuilder::bind(pub1_ip, server_ip)
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

            client.drive(token).await.ok();
            Ok(())
        });
    }

    // Publisher 2: Sends 3 layers
    {
        let token = token.child_token();
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

            client.drive(token).await.ok();
            Ok(())
        });
    }

    // Subscriber: Requests high height for one slot, low for another
    sim.client(sub_ip, async move {
        let mut client = common::client::SimClientBuilder::bind(sub_ip, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .connect("room1")
            .await?;

        client
            .drive_with(|ctx| ctx.discovered_tracks.len() >= 2)
            .await?;

        let mut tracks: Vec<String> = client.ctx.discovered_tracks.iter().cloned().collect();
        tracks.sort();
        let track1 = &tracks[0];
        let track2 = &tracks[1];

        // One slot high (track1), one slot low (track2)
        client.ctx.driver.set_subscriptions(vec![
            Subscription {
                track_id: track1.clone(),
                height: 720,
            },
            Subscription {
                track_id: track2.clone(),
                height: 180,
            },
        ]);

        // Wait for flow on at least one received track
        client
            .drive_until(Duration::from_secs(10), |ctx| {
                ctx.driver
                    .stats()
                    .tracks
                    .values()
                    .all(|t| t.rx_layers.values().any(|l| l.bytes > 0))
            })
            .await?;
        token.cancel();
        Ok(())
    });

    sim.run()?;
    Ok(())
}
