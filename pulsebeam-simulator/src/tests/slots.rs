use super::common;
use pulsebeam::entity::ParticipantId;
use pulsebeam_agent::manager::Subscription;
use pulsebeam_agent::{MediaKind, SimulcastLayer, TransceiverDirection};
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

#[test]
fn slots_layout_update_test() -> turmoil::Result {
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

    // Share publisher track identity (participant_id + mid) with the subscriber.
    let pub1_info: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));
    let pub2_info: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));

    // Publisher 1: Sends "track1"
    {
        let pub1_info = pub1_info.clone();
        sim.client(pub1_ip, async move {
            let mut client = common::client::SimClientBuilder::bind(pub1_ip, server_ip)
                .await?
                .with_track(MediaKind::Video, TransceiverDirection::SendOnly, None)
                .connect("room1")
                .await?;

            // Wait for the local track MID to be advertised by the agent.
            // This can take a few ticks in the async runtime.
            let start = tokio::time::Instant::now();
            while start.elapsed() < Duration::from_secs(10) {
                if !client.local_mids.is_empty() {
                    break;
                }
                // Advance the simulation a bit.
                client.drive_until(Duration::from_millis(50), |_| false).await.ok();
            }

            // Store participant_id + mid for subscriber to use when subscribing.
            if let Some(mid) = client.local_mids.get(0) {
                let pid = client.participant_id.clone();
                *pub1_info.lock().await = Some((pid, mid.to_string()));
            }

            client
                .drive_until(Duration::from_secs(40), |_| false)
                .await
                .ok();

            Ok(())
        });
    }

    // Publisher 2: Sends "track2"
    {
        let pub2_info = pub2_info.clone();
        sim.client(pub2_ip, async move {
            let mut client = common::client::SimClientBuilder::bind(pub2_ip, server_ip)
                .await?
                .with_track(MediaKind::Video, TransceiverDirection::SendOnly, None)
                .connect("room1")
                .await?;

            client
                .drive_until(Duration::from_secs(30), |_| false)
                .await
                .ok();

            if let Some(mid) = client.local_mids.get(0) {
                let pid = client.participant_id.clone();
                *pub2_info.lock().await = Some((pid, mid.to_string()));
            }

            Ok(())
        });
    }

    // Subscriber: Swaps between pub1 and pub2
    sim.client(sub1_ip, async move {
        let mut client = common::client::SimClientBuilder::bind(sub1_ip, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .connect("room1")
            .await?;

        // 1. Wait for both publishers to expose their participant_id + mid.
        let mut info1 = None;
        let mut info2 = None;
        let start = tokio::time::Instant::now();
        while start.elapsed() < Duration::from_secs(40) {
            if info1.is_none() {
                info1 = pub1_info.lock().await.clone();
            }
            if info2.is_none() {
                info2 = pub2_info.lock().await.clone();
            }
            if info1.is_some() && info2.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let (pid1, mid1) = info1.expect("publisher 1 info never populated");
        let (pid2, mid2) = info2.expect("publisher 2 info never populated");

        let track1 = ParticipantId::from_str(&pid1)
            .unwrap()
            .derive_track_id(MediaKind::Video, &mid1)
            .as_str();
        let track2 = ParticipantId::from_str(&pid2)
            .unwrap()
            .derive_track_id(MediaKind::Video, &mid2)
            .as_str();

        tracing::info!("Computed track IDs: {} and {}", track1, track2);

        // 2. Subscribe: Slot 1 -> track1, Slot 2 -> track2
        client
            .agent
            .set_subscriptions(vec![
                Subscription {
                    track_id: track1.to_string(),
                    height: 720,
                },
                Subscription {
                    track_id: track2.to_string(),
                    height: 720,
                },
            ])
            .await?;

        // 3. Wait for assignment to settle and for media to start flowing.
        client
            .wait_for_remote_tracks(2, Duration::from_secs(20))
            .await?;

        tracing::info!("Waiting for media on both slots...");
        client
            .drive_until(Duration::from_secs(20), |stats| {
                stats
                    .tracks
                    .values()
                    .all(|t| t.rx_layers.values().any(|l| l.bytes > 1000))
            })
            .await?;

        // 3. Swap: Slot 1 -> T2, Slot 2 -> T1
        tracing::info!("Swapping slots...");
        client
            .agent
            .set_subscriptions(vec![
                Subscription {
                    track_id: track2.clone(),
                    height: 720,
                },
                Subscription {
                    track_id: track1.clone(),
                    height: 720,
                },
            ])
            .await?;

        // 4. Verify swap
        client
            .drive_until(Duration::from_secs(20), |stats| {
                stats
                    .tracks
                    .values()
                    .all(|t| t.rx_layers.values().any(|l| l.bytes > 5000))
            })
            .await?;

        Ok(())
    });

    common::run_sim_or_timeout(&mut sim, Duration::from_secs(40))?;
    Ok(())
}

#[test]
fn slots_prioritization_test() -> turmoil::Result {
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

    let pub1_info: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));
    let pub2_info: Arc<Mutex<Option<(String, String)>>> = Arc::new(Mutex::new(None));

    // Publisher 1: Sends 3 layers
    {
        let pub1_info = pub1_info.clone();
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
            // Wait for the local track MID to be advertised by the agent.
            let start = tokio::time::Instant::now();
            while start.elapsed() < Duration::from_secs(10) {
                if !client.local_mids.is_empty() {
                    break;
                }
                client.drive_until(Duration::from_millis(50), |_| false).await.ok();
            }

            // Store participant_id + mid for subscriber to use when subscribing.
            if let Some(mid) = client.local_mids.get(0) {
                let pid = client.participant_id.clone();
                *pub1_info.lock().await = Some((pid, mid.to_string()));
            }

            client
                .drive_until(Duration::from_secs(40), |_| false)
                .await
                .ok();

            Ok(())
        });
    }

    // Publisher 2: Sends 3 layers
    {
        let pub2_info = pub2_info.clone();
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
            // Wait for the local track MID to be advertised by the agent.
            let start = tokio::time::Instant::now();
            while start.elapsed() < Duration::from_secs(10) {
                if !client.local_mids.is_empty() {
                    break;
                }
                client.drive_until(Duration::from_millis(50), |_| false).await.ok();
            }

            // Store participant_id + mid for subscriber to use when subscribing.
            if let Some(mid) = client.local_mids.get(0) {
                let pid = client.participant_id.clone();
                *pub2_info.lock().await = Some((pid, mid.to_string()));
            }

            client
                .drive_until(Duration::from_secs(40), |_| false)
                .await
                .ok();

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

        // 1. Wait for both publishers to expose their participant_id + mid.
        let mut info1 = None;
        let mut info2 = None;
        let start = tokio::time::Instant::now();
        while start.elapsed() < Duration::from_secs(40) {
            if info1.is_none() {
                info1 = pub1_info.lock().await.clone();
            }
            if info2.is_none() {
                info2 = pub2_info.lock().await.clone();
            }
            if info1.is_some() && info2.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let (pid1, mid1) = info1.expect("publisher 1 info never populated");
        let (pid2, mid2) = info2.expect("publisher 2 info never populated");

        let track1 = ParticipantId::from_str(&pid1)
            .unwrap()
            .derive_track_id(MediaKind::Video, &mid1)
            .as_str()
            .to_string();
        let track2 = ParticipantId::from_str(&pid2)
            .unwrap()
            .derive_track_id(MediaKind::Video, &mid2)
            .as_str()
            .to_string();

        // One slot high (track1), one slot low (track2)
        client
            .agent
            .set_subscriptions(vec![
                Subscription {
                    track_id: track1.clone(),
                    height: 720,
                },
                Subscription {
                    track_id: track2.clone(),
                    height: 180,
                },
            ])
            .await?;

        // Wait for assignments to take effect and the slots to start receiving media.
        client
            .wait_for_remote_tracks(2, Duration::from_secs(20))
            .await?;

        // Wait for flow
        client
            .drive_until(Duration::from_secs(30), |stats| {
                stats.tracks.len() >= 2 && stats.total_rx_bytes() > 50_000
            })
            .await?;

        let stats = client.agent.get_stats().await.unwrap();
        for (mid, track_stats) in stats.tracks {
            assert!(
                track_stats.rx_layers.values().any(|l| l.bytes > 0),
                "Slot {:?} should have received media",
                mid
            );
        }

        Ok(())
    });

    common::run_sim_or_timeout(&mut sim, Duration::from_secs(40))?;
    Ok(())
}
