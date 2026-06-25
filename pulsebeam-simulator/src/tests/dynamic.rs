use super::common;
use pulsebeam_agent::{MediaKind, TransceiverDirection};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[test]
fn churn_test() {
    common::setup_tracing();

    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(120))
        .rng_seed(0xDEADBEEF)
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let participant1_ip = common::subnet_ip(subnet, 2);
    let participant2_ip = common::subnet_ip(subnet, 3);

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF))
            .await
            .map_err(|e| e.into())
    });
    let done = CancellationToken::new();

    // Participant 1: Stays in the room
    sim.client(participant1_ip, {
        let done = done.clone();
        async move {
            let mut client = common::client::SimClientBuilder::bind(participant1_ip, server_ip)
                .await?
                .with_track(MediaKind::Video, TransceiverDirection::SendOnly, None)
                .connect("room1")
                .await?;

            client.drive(done).await.unwrap();
            Ok(())
        }
    });

    // Participant 2: Joins and leaves multiple times
    sim.client(participant2_ip, async move {
        let _done_guard = done.drop_guard();
        for i in 1..=3 {
            tracing::info!("Participant 2 joining, attempt {}", i);
            let mut client = common::client::SimClientBuilder::bind(participant2_ip, server_ip)
                .await?
                .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
                .connect("room1")
                .await?;

            client
                .drive_until(Duration::from_secs(10), |stats| {
                    let Some(peer) = &stats.peer else {
                        return false;
                    };
                    peer.peer_bytes_rx > 10_000
                })
                .await?;

            tracing::info!("Participant 2 leaving, attempt {}", i);
            client.agent.disconnect().await?;
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        Ok(())
    });

    common::run_sim_or_timeout(&mut sim, Duration::from_secs(130)).expect("Simulation failed");
}

#[test]
fn abrupt_exit_chaos_test() {
    common::setup_tracing();

    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(120))
        .rng_seed(0xC0FFEE)
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let stable_publisher_ip = common::subnet_ip(subnet, 2);
    let stable_subscriber_ip = common::subnet_ip(subnet, 3);

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xC0FFEE))
            .await
            .map_err(|e| e.into())
    });

    let stable_done = CancellationToken::new();

    sim.client(stable_publisher_ip, {
        let stable_done = stable_done.clone();
        async move {
            let mut publisher =
                common::client::SimClientBuilder::bind(stable_publisher_ip, server_ip)
                    .await?
                    .with_track(MediaKind::Video, TransceiverDirection::SendOnly, None)
                    .connect("room1")
                    .await?;

            publisher.drive(stable_done).await?;
            Ok(())
        }
    });

    sim.client(stable_subscriber_ip, async move {
        let _done_guard = stable_done.drop_guard();
        let mut subscriber =
            common::client::SimClientBuilder::bind(stable_subscriber_ip, server_ip)
                .await?
                .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
                .connect("room1")
                .await?;

        subscriber
            .wait_for_discovered_tracks(1, Duration::from_secs(20))
            .await?;

        for _ in 0..6 {
            let before = subscriber.get_stats().total_rx_bytes();

            subscriber
                .drive_until(Duration::from_secs(8), |stats| {
                    stats.total_rx_bytes() > before + 1_500
                })
                .await?;
        }

        Ok(())
    });

    for cycle in 0..8u8 {
        let ip = common::subnet_ip(subnet, 10 + cycle);
        let server_ip_for_client = server_ip;
        let start_delay = Duration::from_secs(3 + (cycle as u64 * 4));

        sim.client(ip, async move {
            tokio::time::sleep(start_delay).await;

            let mut churn_client = common::client::SimClientBuilder::bind(ip, server_ip_for_client)
                .await?
                .with_track(MediaKind::Video, TransceiverDirection::SendOnly, None)
                .connect("room1")
                .await?;

            churn_client.drive_for(Duration::from_secs(2)).await.ok();
            turmoil::partition(ip, server_ip_for_client);
            tokio::time::sleep(Duration::from_millis(500)).await;
            // Intentionally drop without signaling disconnect to model abrupt process exits.
            drop(churn_client);
            turmoil::repair(ip, server_ip_for_client);

            Ok(())
        });
    }

    common::run_sim_or_timeout(&mut sim, Duration::from_secs(125)).expect("Simulation failed");
}
