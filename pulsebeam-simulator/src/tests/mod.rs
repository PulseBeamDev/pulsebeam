use crate::common;
use pulsebeam_agent::{MediaKind, TransceiverDirection};
use std::net::IpAddr;
use std::time::Duration;
use test_strategy::{Arbitrary, proptest};

#[derive(Arbitrary, Debug)]
struct TestInputStruct {}

#[proptest]
fn simulation_test(topo: TestInputStruct) {
    common::setup_tracing();
    let mut sim = turmoil::Builder::new()
        .simulation_duration(Duration::from_secs(15))
        .build();

    let server_ip: IpAddr = "192.168.0.1".parse().unwrap();
    sim.host(server_ip, move || async move {
        common::start_sfu_node(server_ip)
            .await
            .map_err(|e| e.into())
    });

    let ip1: IpAddr = "192.168.1.1".parse().unwrap();
    sim.client(ip1, async move {
        let mut client = common::client::SimClientBuilder::bind(ip1, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::SendOnly)
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly)
            .connect("room1")
            .await?;

        client
            .drive_until(Duration::from_secs(10), |stats| {
                let Some(peer) = &stats.peer else {
                    return false;
                };

                let tx_ok = peer.bytes_tx > 0;
                let rx_ok = peer.bytes_rx > 50_000;
                if tx_ok && rx_ok {
                    tracing::info!("Client 1: Bidirectional flow established!");
                    return true;
                }
                false
            })
            .await?;

        Ok(())
    });

    let ip2: IpAddr = "192.168.2.1".parse().unwrap();
    sim.client(ip2, async move {
        let mut client = common::client::SimClientBuilder::bind(ip2, server_ip)
            .await?
            .with_track(MediaKind::Video, TransceiverDirection::RecvOnly)
            .connect("room1")
            .await?;

        client
            .drive_until(Duration::from_secs(10), |stats| {
                let Some(peer) = &stats.peer else {
                    return false;
                };
                peer.bytes_rx > 50_000
            })
            .await?;

        Ok(())
    });

    sim.run().unwrap();
}
