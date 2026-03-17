use super::common;
use pulsebeam_agent::{MediaKind, TransceiverDirection};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[test]
fn churn_test() {
    common::setup_tracing();

    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(120))
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let participant1_ip = common::subnet_ip(subnet, 2);
    let participant2_ip = common::subnet_ip(subnet, 3);

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip)
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
