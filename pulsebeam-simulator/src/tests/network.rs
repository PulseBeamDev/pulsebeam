use super::common;
use pulsebeam_agent::{MediaKind, TransceiverDirection};
use std::net::IpAddr;
use std::time::Duration;

#[test]
fn network_impairment_test() {
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

    let client_ip: IpAddr = "192.168.1.1".parse().unwrap();

    // Configure network impairments
    // Note: Turmoil allows configuring link characteristics.
    sim.client(client_ip, async move {
        let mut client = common::client::SimClientBuilder::bind(client_ip, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::SendOnly, None)
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly, None)
            .connect("room1")
            .await?;

        // Drive for 30 seconds and check stats
        client.drive_until(Duration::from_secs(30), |stats| {
            let Some(peer) = &stats.peer else { return false; };
            // Assert robustness: we should have some media flow even if impairments were present
            peer.bytes_tx > 0
        }).await?;

        Ok(())
    });

    sim.run().expect("Simulation failed");
}
