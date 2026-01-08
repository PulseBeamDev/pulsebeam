mod common;
use proptest::prelude::*;
use pulsebeam_agent::{MediaKind, TransceiverDirection};
use std::net::IpAddr;
use std::time::Duration;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn simulation_test(peers in 2..=2usize) {
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

        let create_fully_interactive =
            async |server_ip: IpAddr, ip: IpAddr, barrier: std::sync::Arc<tokio::sync::Barrier>| {
                let mut client = common::client::SimClientBuilder::bind(ip, server_ip)
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
                            tracing::info!("Client {}: Bidirectional flow established!", ip);
                            return true;
                        }
                        false
                    })
                    .await?;

                barrier.wait().await;
                Ok(())
            };

        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(peers));
        for i in 1..=peers {
            let ip: IpAddr = format!("192.168.{}.1", i).parse().unwrap();
            let barrier = barrier.clone();
            sim.client(ip, create_fully_interactive(server_ip, ip, barrier));
        }

        sim.run().expect("Simulation failed");
    }
}
