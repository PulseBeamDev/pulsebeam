use super::common;
use pulsebeam_agent::{MediaKind, TransceiverDirection};
use std::time::Duration;

#[test]
fn network_impairment_test() {
    common::setup_tracing();

    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(60))
        .tick_duration(Duration::from_micros(100))
        .rng_seed(0xDEADBEEF)
        .build();

    let subnet = common::reserve_subnet();
    let server_ip = common::subnet_ip(subnet, 1);
    let client_ip = common::subnet_ip(subnet, 2);

    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip, pulsebeam_runtime::rand::seeded_rng(0xDEADBEEF))
            .await
            .map_err(|e| e.into())
    });

    sim.client(client_ip, async move {
        let mut client = common::client::SimClientBuilder::bind(client_ip, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::SendOnly, None)
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .connect("room1")
            .await?;

        // Phase 1: verify baseline flow
        client.drive_for(Duration::from_secs(30)).await?;
        let stats = client.agent.get_stats().await.unwrap_or_default();
        assert!(
            stats.peer.as_ref().map_or(false, |p| p.peer_bytes_tx > 0),
            "no outbound bytes after baseline drive"
        );

        // Phase 2: apply impairments and ensure we still see some flow.
        turmoil::partition(client_ip, server_ip);
        client.drive_for(Duration::from_secs(10)).await?;

        // Phase 3: repair and ensure we can still send/receive.
        turmoil::repair(client_ip, server_ip);
        client.drive_for(Duration::from_secs(30)).await?;
        let stats = client.agent.get_stats().await.unwrap_or_default();
        assert!(
            stats.peer.as_ref().map_or(false, |p| p.peer_bytes_tx > 0),
            "no outbound bytes after repair"
        );

        Ok(())
    });

    sim.run().expect("Simulation failed");
}
